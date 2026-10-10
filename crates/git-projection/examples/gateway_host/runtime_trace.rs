// SPDX-License-Identifier: Apache-2.0
//! Secret-free process-local diagnostics. These are measurements, not billing records.
//! RSS/high-water are process-wide; Linux I/O counters are not network byte counters.
use serde_json::json;
use std::{
    cell::Cell,
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};
thread_local! { static REQUEST: Cell<u64> = const { Cell::new(0) }; }
static NEXT: AtomicU64 = AtomicU64::new(1);
#[derive(Default)]
struct Counters {
    read: u64,
    write: u64,
    rchar: u64,
    wchar: u64,
    rss: u64,
    high_water: u64,
    cpu_us: u64,
    child_cpu_us: u64,
}
fn field(raw: &str, name: &str) -> u64 {
    raw.lines()
        .find_map(|line| {
            line.strip_prefix(name)
                .and_then(|s| s.split_whitespace().next())
                .and_then(|s| s.parse().ok())
        })
        .unwrap_or(0)
}
fn cpu_us(children: bool) -> u64 {
    #[cfg(unix)]
    {
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
        // The pointer is valid and initialized before successful reads.
        if unsafe {
            libc::getrusage(
                if children {
                    libc::RUSAGE_CHILDREN
                } else {
                    libc::RUSAGE_SELF
                },
                usage.as_mut_ptr(),
            )
        } == 0
        {
            let r = unsafe { usage.assume_init() };
            return (r.ru_utime.tv_sec as u64 + r.ru_stime.tv_sec as u64) * 1_000_000
                + r.ru_utime.tv_usec as u64
                + r.ru_stime.tv_usec as u64;
        }
    }
    0
}
fn counters() -> Counters {
    let io = std::fs::read_to_string("/proc/self/io").unwrap_or_default();
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    Counters {
        read: field(&io, "read_bytes:"),
        write: field(&io, "write_bytes:"),
        rchar: field(&io, "rchar:"),
        wchar: field(&io, "wchar:"),
        rss: field(&status, "VmRSS:") * 1024,
        high_water: field(&status, "VmHWM:") * 1024,
        cpu_us: cpu_us(false),
        child_cpu_us: cpu_us(true),
    }
}
pub struct Span {
    phase: &'static str,
    start: Instant,
    before: Counters,
    result: &'static str,
}
impl Span {
    pub fn request() -> Self {
        REQUEST.set(NEXT.fetch_add(1, Ordering::Relaxed));
        Self::new("request")
    }
    pub fn new(phase: &'static str) -> Self {
        Self {
            phase,
            start: Instant::now(),
            before: counters(),
            result: "finished",
        }
    }
    pub fn outcome(&mut self, success: bool) {
        self.result = if success { "ok" } else { "denied_or_failed" };
    }
}
impl Drop for Span {
    fn drop(&mut self) {
        let after = counters();
        eprintln!(
            "{}",
            json!({"event":"gateway_phase","request":REQUEST.get(),"phase":self.phase,"outcome":self.result,"wall_ms":self.start.elapsed().as_secs_f64()*1000.,"process_cpu_ms":after.cpu_us.saturating_sub(self.before.cpu_us) as f64/1000.,"children_cpu_ms":after.child_cpu_us.saturating_sub(self.before.child_cpu_us) as f64/1000.,"disk_read_bytes":after.read.saturating_sub(self.before.read),"disk_write_bytes":after.write.saturating_sub(self.before.write),"io_read_chars":after.rchar.saturating_sub(self.before.rchar),"io_write_chars":after.wchar.saturating_sub(self.before.wchar),"process_rss_bytes":after.rss,"process_high_water_bytes":after.high_water})
        );
    }
}
/// Values must be byte counts or bounded counts, never IDs, paths, account labels or credentials.
pub fn count(name: &'static str, value: usize) {
    eprintln!(
        "{}",
        json!({"event":"gateway_count","request":REQUEST.get(),"name":name,"value":value})
    );
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn proc_fields_are_exact_and_numeric() {
        assert_eq!(field("VmRSS: 42 kB\n", "VmRSS:"), 42);
        assert_eq!(field("OtherVmRSS: 42\n", "VmRSS:"), 0);
        assert_eq!(field("VmRSS: invalid\n", "VmRSS:"), 0);
    }
}
