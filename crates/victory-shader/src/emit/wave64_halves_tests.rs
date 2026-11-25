//! A wave64 console group of 64 threads on every hardware adapter. Where the
//! host has no 64-lane waves it runs as two 32-lane halves in the console
//! flow (`lower::CONSOLE_FLOW`). Hand-assembled GFX10 programs, each lane's
//! result checked against what the console computes (RDNA2 ISA):
//!
//! - LDS the lanes share without a barrier (the console compiler leaves it
//!   out inside one wave): lane t writes its id, then reads lane 63 - t's;
//! - a whole-wave section (`s_or_saveexec_b64 -1`): the sum of all 64
//!   lanes' ids, read lane by lane and added in scalars, in every lane;
//! - the same section's prefix over halves is covered by the first test's
//!   exchange of totals when it lands in a reduction (see `wave::Shape`).

use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
use windows::Win32::Graphics::Direct3D12::{D3D12CreateDevice, ID3D12Device};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, IDXGIAdapter1, IDXGIFactory1,
};

use super::host::{Dispatch, Host, Root, View};
use crate::native::{ComputeIo, RootParam, RootType, StageIo};

fn program(words: &[u32], lds: bool) -> crate::NativeProgram {
    crate::NativeProgram {
        stage: crate::HwStage::Cs,
        wave64: true,
        vgprs: 8,
        layout: vec![RootParam {
            root: RootType::Table,
            table_offset_dwords: 0,
            sgprs: 1,
            base_register: 0,
            user_data: 0,
            slot: 0,
        }],
        used: vec![],
        io: StageIo::Cs(ComputeIo {
            // one user SGPR (the UAV table); one 512-byte LDS granule
            rsrc2: 1 << 1 | u32::from(lds) << 15,
            threads: [64, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

/// The preamble loading the UAV descriptor into s[4:7].
const LOAD: [u32; 5] = [
    0xBE82_0300, // s_mov_b32 s2, s0
    0xBE83_0380, // s_mov_b32 s3, 0
    0xF408_0101,
    0xFA00_0000, // s_load_dwordx4 s[4:7], s[2:3], null
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
];
/// Stores v2 to dword `thread id` of the UAV and ends.
const STORE: [u32; 4] = [
    0x3406_0082, // v_lshlrev_b32 v3, 2, v0
    0xE070_1000,
    0x8001_0203, // buffer_store_dword v2, v3, s[4:7], 0 offen
    0xBF81_0000, // s_endpgm
];

/// Lane t writes t to LDS dword t, then reads dword 63 - t, no barrier.
fn lds_mirror() -> Vec<u32> {
    let mut w = LOAD.to_vec();
    w.extend([
        0x3402_0082, // v_lshlrev_b32 v1, 2, v0
        0xD834_0000,
        0x0000_0001, // ds_write_b32 v1, v0
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        0x4C04_00BF, // v_sub_nc_u32 v2, 63, v0
        0x3404_0482, // v_lshlrev_b32 v2, 2, v2
        0xD8D8_0000,
        0x0200_0002, // ds_read_b32 v2, v2
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
    ]);
    w.extend(STORE);
    w
}

/// The sum of the thread ids of all 64 lanes, in every lane: the sections
/// the console compiler writes for `WaveActiveSum` (each half of 32 lanes
/// summed with DPP and `v_permlanex16`, then lanes 0 and 32 read and added).
fn whole_wave_sum() -> Vec<u32> {
    let mut w = LOAD.to_vec();
    w.extend([
        0xBE8A_25C1, // s_or_saveexec_b64 s[10:11], -1
        0xD501_0001,
        0x002A_0080, // v_cndmask_b32_e64 v1, 0, v0, s[10:11]
        0x4A02_02FA,
        0xFF01_6101, // v_add_nc_u32_dpp v1, v1, v1 row_xmask:1
        0x4A02_02FA,
        0xFF01_6201, // v_add_nc_u32_dpp v1, v1, v1 row_xmask:2
        0x4A02_02FA,
        0xFF01_6401, // v_add_nc_u32_dpp v1, v1, v1 row_xmask:4
        0x4A02_02FA,
        0xFF01_6801, // v_add_nc_u32_dpp v1, v1, v1 row_xmask:8
        0xD778_0004,
        0x0305_8301, // v_permlanex16_b32 v4, v1, -1, -1
        0x4A02_0901, // v_add_nc_u32_e32 v1, v1, v4
        0xD760_000C,
        0x0001_0101, // v_readlane_b32 s12, v1, 0
        0xD760_000D,
        0x0001_4101, // v_readlane_b32 s13, v1, 32
        0x800C_0D0C, // s_add_u32 s12, s12, s13
        0xBEFE_040A, // s_mov_b64 exec, s[10:11]
        0x7E04_020C, // v_mov_b32 v2, s12
    ]);
    w.extend(STORE);
    w
}

/// Every hardware adapter's device, by name.
fn devices() -> Vec<(String, Host)> {
    let mut out = Vec::new();
    // SAFETY: plain DXGI and D3D12 calls with valid out pointers.
    unsafe {
        let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else {
            return out;
        };
        let mut i = 0;
        while let Ok(a) = factory.EnumAdapters1(i) {
            i += 1;
            let a: IDXGIAdapter1 = a;
            let Ok(d) = a.GetDesc1() else { continue };
            if d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
                continue;
            }
            let n = d.Description.iter().position(|&c| c == 0).unwrap_or(128);
            let name = String::from_utf16_lossy(&d.Description[..n]);
            let mut device: Option<ID3D12Device> = None;
            if D3D12CreateDevice(&a, D3D_FEATURE_LEVEL_12_0, &mut device).is_ok()
                && let Some(device) = device
            {
                out.push((name, Host { device }));
            }
        }
    }
    out
}

fn emit(native: &crate::NativeProgram, target: super::Target) -> super::HostShader {
    let c = crate::Container {
        digest: [0; 16],
        version: (1, 0),
        parts: vec![],
    };
    let p = crate::ir::build(&native.code, native.wave64, native.vgprs).unwrap();
    let res = crate::resource::track(&p, &native.layout, native.stage).unwrap();
    let io = crate::io::raise(&p, native).unwrap();
    let st = crate::structure::structure(&p).unwrap();
    super::emit(&c, native, &p, &st, &io, &res, target).unwrap()
}

/// Runs `native` on each adapter; `want(t)` is lane t's result.
fn check(native: crate::NativeProgram, want: impl Fn(u32) -> u32) {
    let mut ran = 0;
    let mut wrong = Vec::new();
    for (name, host) in devices() {
        // 16-lane waves (an integrated GPU) run neither form.
        if !host.target().runs(32) && !host.target().runs(64) {
            eprintln!("{name}: skipped");
            continue;
        }
        let cs = emit(&native, host.target());
        let (root, pso) = host
            .compute_pipeline(&cs)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        let views = [View::RawUav(64)];
        let data = vec![0; (cs.user_data_dwords() as usize).max(1)];
        let out = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![(0, Root::Constants(data))],
                    views: &views,
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .unwrap()
            .remove(0);
        ran += 1;
        let bad: Vec<String> = (0..64)
            .filter(|&t| out[t as usize] != want(t))
            .map(|t| format!("{t}: {} (want {})", out[t as usize], want(t)))
            .collect();
        if bad.is_empty() {
            eprintln!("{name}: exact");
        } else {
            eprintln!("{name}: {} lanes wrong: {}", bad.len(), bad.join(", "));
            wrong.push(name);
        }
    }
    assert!(ran > 0, "no adapter ran the program");
    assert!(wrong.is_empty(), "wrong on {wrong:?}");
}

#[test]
fn lds_shared_between_the_halves_without_a_barrier() {
    check(program(&lds_mirror(), true), |t| 63 - t);
}

#[test]
fn whole_wave_sum_covers_both_halves() {
    check(program(&whole_wave_sum(), false), |_| 63 * 64 / 2);
}
