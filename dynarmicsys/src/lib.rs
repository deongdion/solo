//! FFI to a pinned dynarmic fork through a C shim (`shim/shim.h`).

#![allow(non_camel_case_types)]

use std::ffi::c_void;

/// Opaque CPU handle owned by the shim.
#[repr(C)]
pub struct DynCpu {
    _private: [u8; 0],
}

pub const DYN_HALT_EXTERNAL: u32 = 0x0100_0000;
pub const DYN_HALT_SVC: u32 = 0x0200_0000;
pub const DYN_HALT_EXCEPTION: u32 = 0x0400_0000;
pub const DYN_HALT_FAULT: u32 = 0x0800_0000;
pub const DYN_HALT_ERROR: u32 = 0x8000_0000;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct DynHooks {
    pub user: *mut c_void,
    pub on_svc: Option<unsafe extern "C" fn(user: *mut c_void, imm: u32) -> i32>,
    pub on_exception: Option<unsafe extern "C" fn(user: *mut c_void, pc: u64, kind: u32)>,
    pub on_fault: Option<unsafe extern "C" fn(user: *mut c_void, address: u64, size: u32, is_write: u32)>,
    pub can_execute: Option<unsafe extern "C" fn(user: *mut c_void, address: u64) -> i32>,
    pub on_access: Option<unsafe extern "C" fn(user: *mut c_void, address: u64, size: u32, kind: u32, data: *mut u8) -> i32>,
}

pub const DYN_PRECISE_FAULTS: u32 = 0x1;

unsafe extern "C" {
    pub fn dyn_new(
        is64: i32,
        page_table: *mut *mut u8,
        address_bits: u32,
        code_cache_size: u64,
        flags: u32,
        hooks: *const DynHooks,
    ) -> *mut DynCpu;
    pub fn dyn_free(cpu: *mut DynCpu);
    pub fn dyn_run(cpu: *mut DynCpu, budget: u64, executed: *mut u64) -> u32;
    pub fn dyn_halt(cpu: *mut DynCpu);
    pub fn dyn_step(cpu: *mut DynCpu) -> u32;
    pub fn dyn_reg(cpu: *const DynCpu, index: u32) -> u64;
    pub fn dyn_set_reg(cpu: *mut DynCpu, index: u32, value: u64);
    pub fn dyn_invalidate(cpu: *mut DynCpu, address: u64, length: u64);
    pub fn dyn_clear_exclusive(cpu: *mut DynCpu);
}
