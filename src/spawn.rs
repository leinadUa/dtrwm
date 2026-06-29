use libc;
use std::ffi::CString;
use std::io::Write;

pub fn run(cmd: &str) {
    let args: Vec<&str> = cmd.split_whitespace().collect();
    if args.is_empty() {
        return;
    }
    let c_args: Vec<CString> = match args.iter().map(|s| CString::new(*s)).collect::<Result<Vec<_>, _>>() {
        Ok(v) => v,
        Err(_) => return,
    };
    let mut ptrs: Vec<*const libc::c_char> = c_args.iter().map(|s| s.as_ptr()).collect();
    ptrs.push(std::ptr::null());

    unsafe {
        let pid = libc::fork();
        if pid < 0 {
            log::error!("fork() failed");
            return;
        }
        if pid == 0 {
            libc::setsid();
            let pid2 = libc::fork();
            if pid2 == 0 {
                let log_path = CString::new("/tmp/dtrwm_child.log").unwrap();
                let fd = libc::open(
                    log_path.as_ptr(),
                    libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND,
                    0o644,
                );
                if fd >= 0 {
                    libc::dup2(fd, libc::STDOUT_FILENO);
                    libc::dup2(fd, libc::STDERR_FILENO);
                    libc::close(fd);
                }

                libc::execvp(ptrs[0], ptrs.as_ptr());

                let err = std::io::Error::last_os_error();
                let msg = format!("execvp({cmd}) failed: {err}\n");
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true).append(true)
                    .open("/tmp/dtrwm_spawn.log")
                {
                    let _ = f.write_all(msg.as_bytes());
                }
                libc::_exit(1);
            }
            libc::_exit(0);
        }
        let mut status: libc::c_int = 0;
        libc::waitpid(pid, &mut status, 0);
    }
}
