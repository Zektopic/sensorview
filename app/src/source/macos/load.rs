//! CPU load and memory pressure, via the public Mach host interfaces.
//!
//! Nothing here is private API — `host_processor_info` and `host_statistics64`
//! are stable and unprivileged. Load is a *rate*, so the per-core tick counters
//! must be differenced against the previous poll; the first snapshot after
//! startup therefore reports no load rather than a meaningless
//! since-boot average.

use mach2::kern_return::KERN_SUCCESS;
use mach2::mach_types::host_t;
use mach2::message::mach_msg_type_number_t;
use mach2::traps::mach_task_self;
use mach2::vm_types::natural_t;

use crate::model::{Sensor, SensorType};

use super::sensor;

const CPU_STATE_MAX: usize = 4;
const CPU_STATE_USER: usize = 0;
const CPU_STATE_SYSTEM: usize = 1;
const CPU_STATE_IDLE: usize = 2;
const CPU_STATE_NICE: usize = 3;
const PROCESSOR_CPU_LOAD_INFO: libc::c_int = 2;
const HOST_VM_INFO64: libc::c_int = 4;

extern "C" {
    fn mach_host_self() -> host_t;
    fn host_processor_info(
        host: host_t,
        flavor: libc::c_int,
        out_processor_count: *mut natural_t,
        out_processor_info: *mut *mut libc::c_int,
        out_processor_infoCnt: *mut mach_msg_type_number_t,
    ) -> libc::c_int;
    fn vm_deallocate(target: u32, address: usize, size: usize) -> libc::c_int;
    fn host_statistics64(
        host_priv: host_t,
        flavor: libc::c_int,
        host_info_out: *mut libc::c_int,
        host_info_outCnt: *mut mach_msg_type_number_t,
    ) -> libc::c_int;
}

/// Subset of `vm_statistics64_data_t` we actually read. Declared locally with
/// the full leading field order so the offsets line up with the kernel's
/// struct; trailing fields we don't use are simply not named.
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct VmStatistics64 {
    free_count: natural_t,
    active_count: natural_t,
    inactive_count: natural_t,
    wire_count: natural_t,
    zero_fill_count: u64,
    reactivations: u64,
    pageins: u64,
    pageouts: u64,
    faults: u64,
    cow_faults: u64,
    lookups: u64,
    hits: u64,
    purges: u64,
    purgeable_count: natural_t,
    speculative_count: natural_t,
    decompressions: u64,
    compressions: u64,
    swapins: u64,
    swapouts: u64,
    compressor_page_count: natural_t,
    throttled_count: natural_t,
    external_page_count: natural_t,
    internal_page_count: natural_t,
    total_uncompressed_pages_in_compressor: u64,
}

pub struct LoadCollector {
    /// Per-core cumulative ticks from the previous poll, for differencing.
    prev_ticks: Vec<[u32; CPU_STATE_MAX]>,
    page_size: u64,
}

impl LoadCollector {
    pub fn new() -> Self {
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        Self {
            prev_ticks: Vec::new(),
            // sysconf returns -1 on failure; 16 KiB is the Apple Silicon page
            // size and a far better guess than 0 (which would divide by zero).
            page_size: if page_size > 0 { page_size as u64 } else { 16384 },
        }
    }

    /// Per-core load percentages, newest deltas against the last call.
    /// Empty on the very first call (no baseline to difference against).
    fn per_core_load(&mut self) -> Vec<f32> {
        let mut count: natural_t = 0;
        let mut info: *mut libc::c_int = std::ptr::null_mut();
        let mut info_count: mach_msg_type_number_t = 0;

        let kr = unsafe {
            host_processor_info(
                mach_host_self(),
                PROCESSOR_CPU_LOAD_INFO,
                &mut count,
                &mut info,
                &mut info_count,
            )
        };
        if kr != KERN_SUCCESS || info.is_null() || count == 0 {
            return Vec::new();
        }

        let ticks: Vec<[u32; CPU_STATE_MAX]> = (0..count as usize)
            .map(|cpu| {
                let mut states = [0u32; CPU_STATE_MAX];
                for (state, slot) in states.iter_mut().enumerate() {
                    // SAFETY: the kernel guarantees CPU_STATE_MAX ints per CPU.
                    *slot = unsafe { *info.add(cpu * CPU_STATE_MAX + state) } as u32;
                }
                states
            })
            .collect();

        // The kernel vm_allocate'd this buffer for us; it is ours to free.
        unsafe {
            vm_deallocate(
                mach_task_self(),
                info as usize,
                info_count as usize * std::mem::size_of::<libc::c_int>(),
            );
        }

        let loads = if self.prev_ticks.len() == ticks.len() {
            ticks
                .iter()
                .zip(&self.prev_ticks)
                .map(|(now, before)| {
                    // Counters are u32 and do wrap on long uptimes; wrapping_sub
                    // keeps a wrap from producing a huge bogus delta.
                    let delta = |i: usize| now[i].wrapping_sub(before[i]) as u64;
                    let busy = delta(CPU_STATE_USER) + delta(CPU_STATE_SYSTEM) + delta(CPU_STATE_NICE);
                    let total = busy + delta(CPU_STATE_IDLE);
                    if total == 0 {
                        0.0
                    } else {
                        (busy as f32 / total as f32 * 100.0).clamp(0.0, 100.0)
                    }
                })
                .collect()
        } else {
            Vec::new()
        };

        self.prev_ticks = ticks;
        loads
    }

    /// CPU load sensors: one total plus one per core.
    pub fn cpu_load_sensors(&mut self) -> Vec<Sensor> {
        let loads = self.per_core_load();
        if loads.is_empty() {
            return Vec::new();
        }
        let total = loads.iter().sum::<f32>() / loads.len() as f32;

        let mut out = vec![sensor(
            "/applesoc/0/load/0",
            "Total CPU Usage",
            SensorType::Load,
            0,
            total,
        )];
        out.extend(loads.iter().enumerate().map(|(i, load)| {
            sensor(
                &format!("/applesoc/0/load/{}", i + 1),
                &format!("CPU Core #{}", i + 1),
                SensorType::Load,
                i as u32 + 1,
                *load,
            )
        }));
        out
    }

    /// Memory sensors, using Activity Monitor's notion of "used": application
    /// memory (internal minus purgeable) plus wired plus compressed. Raw
    /// `free_count` badly overstates availability on macOS because the OS
    /// deliberately keeps memory populated.
    pub fn memory_sensors(&self) -> Vec<Sensor> {
        let mut stats = VmStatistics64::default();
        // The flavor's count is expressed in 32-bit words.
        let mut count = (std::mem::size_of::<VmStatistics64>() / std::mem::size_of::<libc::c_int>())
            as mach_msg_type_number_t;

        let kr = unsafe {
            host_statistics64(
                mach_host_self(),
                HOST_VM_INFO64,
                (&mut stats as *mut VmStatistics64).cast(),
                &mut count,
            )
        };
        if kr != KERN_SUCCESS {
            return Vec::new();
        }

        let page = self.page_size;
        let app = (stats.internal_page_count as u64).saturating_sub(stats.purgeable_count as u64);
        let used_bytes =
            (app + stats.wire_count as u64 + stats.compressor_page_count as u64) * page;
        let total_bytes = crate::sysinfo::sysctl_u64("hw.memsize").unwrap_or(0);
        if total_bytes == 0 {
            return Vec::new();
        }

        const GB: f64 = 1024.0 * 1024.0 * 1024.0;
        let used_gb = used_bytes as f64 / GB;
        let total_gb = total_bytes as f64 / GB;

        vec![
            sensor(
                "/ram/0/load/0",
                "Memory Used",
                SensorType::Load,
                0,
                (used_gb / total_gb * 100.0) as f32,
            ),
            sensor("/ram/0/data/0", "Memory Used", SensorType::Data, 0, used_gb as f32),
            sensor(
                "/ram/0/data/1",
                "Memory Available",
                SensorType::Data,
                1,
                (total_gb - used_gb).max(0.0) as f32,
            ),
            sensor(
                "/ram/0/data/2",
                "Compressed",
                SensorType::Data,
                2,
                (stats.compressor_page_count as u64 * page) as f64 as f32 / GB as f32,
            ),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_sensors_are_plausible() {
        let collector = LoadCollector::new();
        let sensors = collector.memory_sensors();
        assert!(!sensors.is_empty(), "host_statistics64 should work unprivileged");

        let load = sensors.iter().find(|s| s.sensor_type == SensorType::Load).unwrap();
        let pct = load.value.unwrap();
        assert!((0.0..=100.0).contains(&pct), "memory load {pct} out of range");

        // Used must be a real fraction of installed RAM, not a page count.
        let used = sensors
            .iter()
            .find(|s| s.name == "Memory Used" && s.sensor_type == SensorType::Data)
            .unwrap()
            .value
            .unwrap();
        assert!(used > 0.1, "used memory {used} GB implausibly small");
        assert!(used < 1024.0, "used memory {used} GB implausibly large");
    }

    #[test]
    fn first_load_poll_has_no_baseline_then_reports() {
        let mut collector = LoadCollector::new();
        // No previous ticks to difference against yet.
        assert!(collector.cpu_load_sensors().is_empty());

        std::thread::sleep(std::time::Duration::from_millis(120));
        let sensors = collector.cpu_load_sensors();
        assert!(!sensors.is_empty(), "second poll should produce load");
        // One total + one per logical core.
        let cores = crate::sysinfo::sysctl_u64("hw.logicalcpu").unwrap_or(0) as usize;
        assert_eq!(sensors.len(), cores + 1);
        for s in &sensors {
            let v = s.value.unwrap();
            assert!((0.0..=100.0).contains(&v), "load {v} out of range for {}", s.name);
        }
    }
}
