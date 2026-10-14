//! 专用 Holo 介质上的 EINTR/DMA 生命周期验证；会留下未索引尾部，须从诊断前备份恢复。
use std::io::{Cursor, Read};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};
use tape_rs::ltfs::volume::LtfsVolume;
use tape_rs::scsi::device::ScsiDevice;
use tape_rs::scsi::inquiry::read_unit_serial;

extern "C" fn interrupted(_: i32) {}

struct ArmedReader<R> {
    inner: R,
    armed: Arc<AtomicBool>,
}
impl<R: Read> Read for ArmedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.armed.store(true, Ordering::Release);
        Ok(n)
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    if std::env::var("TAPE_RS_DIRECT_INTERRUPT").as_deref() != Ok("PF2701L08-backed-up") {
        return Err("仅允许已备份的专用 PF2701L08；测试会留下未索引尾部".into());
    }
    let dev = ScsiDevice::open("/dev/sg7")?;
    if read_unit_serial(&dev).as_deref() != Some("IBMlisa42299") {
        return Err("驱动器身份不符".into());
    }
    let mut vol = LtfsVolume::mount(&dev)?;
    if vol.label().volume_uuid.to_string() != "5c16daae-fa54-40d8-ba28-988f3e9fd15a" {
        return Err("介质 UUID 不符".into());
    }
    let path = "io-direct-interrupt-20260927/data";
    if vol.index().find_file(path).is_some() {
        return Err("诊断路径已存在".into());
    }
    let action = SigAction::new(
        SigHandler::Handler(interrupted),
        SaFlags::empty(),
        SigSet::empty(),
    );
    // 处理器不访问任何状态；只为中断本诊断进程中的同步 ioctl。
    let old = unsafe { sigaction(Signal::SIGUSR1, &action)? };
    let current = unsafe { nix::libc::pthread_self() };
    let done = Arc::new(AtomicBool::new(false));
    let stop = done.clone();
    let armed = Arc::new(AtomicBool::new(false));
    let ready = armed.clone();
    let mut source = ArmedReader {
        inner: Cursor::new(vec![0x69; 16 << 20]),
        armed,
    };
    let sender = std::thread::spawn(move || {
        while !stop.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(1));
            // 主线程在设置 done、join 完成之前不会退出。
            if ready.load(Ordering::Acquire) {
                unsafe {
                    nix::libc::pthread_kill(current, nix::libc::SIGUSR1);
                }
            }
        }
    });
    let result = vol.append_file(path, &mut source);
    done.store(true, Ordering::Release);
    sender.join().map_err(|_| "信号线程失败")?;
    unsafe {
        sigaction(Signal::SIGUSR1, &old)?;
    }
    let stats = dev.direct_io_stats();
    println!(
        "result={result:?}; write_direct={}; write_indirect={}; writable={}",
        stats.write.direct,
        stats.write.indirect,
        vol.writable()
    );
    if !matches!(
        result,
        Err(tape_rs::error::TapeError::Nix(nix::errno::Errno::EINTR))
    ) {
        return Err("未复现 SG_IO EINTR，不能将本次算为生命周期通过".into());
    }
    if dev.execute_no_data(&[0, 0, 0, 0, 0, 0], 1000).is_ok() {
        return Err("结果未定的句柄没有冻结".into());
    }
    println!("PASS: SG_IO interrupted; pending DMA drained before return; no CDB replay");
    Ok(())
}
