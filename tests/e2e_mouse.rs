use std::env;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[test]
fn mouse_double_click_switches_session_in_terminal() {
    let temp_dir = temp_dir("mouse");
    fs::create_dir_all(&temp_dir).unwrap();
    let tmux_bin = temp_dir.join("tmux");
    let sessions_file = temp_dir.join("sessions");
    let switch_file = temp_dir.join("switched");
    let pin_file = temp_dir.join("pins");
    write_fake_tmux(&tmux_bin);
    write_sessions(&sessions_file, &[("current", 100), ("clicked", 200)]);

    let (mut master, slave) = open_pty(24, 80);
    let mut child = spawn_picker(
        &temp_dir,
        &sessions_file,
        "current",
        &switch_file,
        &pin_file,
        slave,
    );

    wait_for_output(&mut master, "clicked", Duration::from_secs(2));
    master.write_all(b"\x1b[<0;45;21M\x1b[<0;45;21M").unwrap();
    master.flush().unwrap();

    let switched = wait_for_file(
        &switch_file,
        Duration::from_secs(2),
        &mut master,
        &mut child,
    );
    assert_eq!(switched.trim(), "clicked");

    let status = child.wait().unwrap();
    assert!(status.success());

    let _ = fs::remove_dir_all(temp_dir);
}

#[test]
fn mouse_wheel_scrolls_session_list_in_terminal() {
    let temp_dir = temp_dir("wheel");
    fs::create_dir_all(&temp_dir).unwrap();
    let tmux_bin = temp_dir.join("tmux");
    let sessions_file = temp_dir.join("sessions");
    let switch_file = temp_dir.join("switched");
    let pin_file = temp_dir.join("pins");
    let sessions = (1..=30)
        .map(|index| (format!("s{index:02}"), 10_000 - index))
        .collect::<Vec<_>>();
    write_fake_tmux(&tmux_bin);
    write_sessions(
        &sessions_file,
        &sessions
            .iter()
            .map(|(name, activity)| (name.as_str(), *activity))
            .collect::<Vec<_>>(),
    );

    let (mut master, slave) = open_pty(24, 80);
    let mut child = spawn_picker(
        &temp_dir,
        &sessions_file,
        "s01",
        &switch_file,
        &pin_file,
        slave,
    );

    wait_for_output(&mut master, "s18", Duration::from_secs(2));
    master.write_all(b"\x1b[<65;45;10M").unwrap();
    master.flush().unwrap();
    wait_for_output(&mut master, "s21", Duration::from_secs(2));

    master.write_all(b"q").unwrap();
    master.flush().unwrap();
    let status = child.wait().unwrap();
    assert!(status.success());

    let _ = fs::remove_dir_all(temp_dir);
}

#[test]
fn right_click_pins_session_in_terminal() {
    let temp_dir = temp_dir("right-click");
    fs::create_dir_all(&temp_dir).unwrap();
    let tmux_bin = temp_dir.join("tmux");
    let sessions_file = temp_dir.join("sessions");
    let switch_file = temp_dir.join("switched");
    let pin_file = temp_dir.join("pins");
    write_fake_tmux(&tmux_bin);
    write_sessions(&sessions_file, &[("current", 100), ("clicked", 200)]);

    let (mut master, slave) = open_pty(24, 80);
    let mut child = spawn_picker(
        &temp_dir,
        &sessions_file,
        "current",
        &switch_file,
        &pin_file,
        slave,
    );

    wait_for_output(&mut master, "clicked", Duration::from_secs(2));
    master.write_all(b"\x1b[<2;45;21M").unwrap();
    master.flush().unwrap();
    wait_for_file_contents(
        &pin_file,
        "clicked\n",
        Duration::from_secs(2),
        &mut master,
        &mut child,
    );

    master.write_all(b"q").unwrap();
    master.flush().unwrap();
    let status = child.wait().unwrap();
    assert!(status.success());

    let _ = fs::remove_dir_all(temp_dir);
}

#[test]
fn left_drag_reorders_pinned_session_in_terminal() {
    let temp_dir = temp_dir("drag");
    fs::create_dir_all(&temp_dir).unwrap();
    let tmux_bin = temp_dir.join("tmux");
    let sessions_file = temp_dir.join("sessions");
    let switch_file = temp_dir.join("switched");
    let pin_file = temp_dir.join("pins");
    write_fake_tmux(&tmux_bin);
    write_sessions(&sessions_file, &[("current", 100), ("clicked", 200)]);
    fs::write(&pin_file, "current\nclicked\n").unwrap();

    let (mut master, slave) = open_pty(24, 80);
    let mut child = spawn_picker(
        &temp_dir,
        &sessions_file,
        "current",
        &switch_file,
        &pin_file,
        slave,
    );

    wait_for_output(&mut master, "clicked", Duration::from_secs(2));
    master.write_all(b"\x1b[<0;5;22M\x1b[<32;5;21M").unwrap();
    master.flush().unwrap();
    wait_for_file_contents(
        &pin_file,
        "clicked\ncurrent\n",
        Duration::from_secs(2),
        &mut master,
        &mut child,
    );

    master.write_all(b"q").unwrap();
    master.flush().unwrap();
    let status = child.wait().unwrap();
    assert!(status.success());

    let _ = fs::remove_dir_all(temp_dir);
}

#[test]
fn ctrl_j_selects_collapsed_group_for_expansion() {
    let temp_dir = temp_dir("ctrl-group");
    fs::create_dir_all(&temp_dir).unwrap();
    let tmux_bin = temp_dir.join("tmux");
    let sessions_file = temp_dir.join("sessions");
    let group_file = temp_dir.join("groups.toml");
    let switch_file = temp_dir.join("switched");
    let pin_file = temp_dir.join("pins");
    write_fake_tmux(&tmux_bin);
    write_sessions(&sessions_file, &[("current", 100), ("clicked", 200)]);
    fs::write(
        &group_file,
        r#"version = 1

[[groups]]
name = "Work"
collapsed = true
sessions = ["current"]

[[groups]]
name = "Personal"
collapsed = true
sessions = ["clicked"]
"#,
    )
    .unwrap();

    let (mut master, slave) = open_pty(24, 80);
    let mut child = spawn_picker(
        &temp_dir,
        &sessions_file,
        "current",
        &switch_file,
        &pin_file,
        slave,
    );

    wait_for_output(&mut master, "Personal", Duration::from_secs(2));
    master.write_all(b"\x0c\n\r").unwrap();
    master.flush().unwrap();
    wait_for_output(&mut master, "clicked", Duration::from_secs(2));

    master.write_all(b"q").unwrap();
    master.flush().unwrap();
    let status = child.wait().unwrap();
    assert!(status.success());

    let _ = fs::remove_dir_all(temp_dir);
}

fn temp_dir(suffix: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    env::temp_dir().join(format!("tmux-session-picker-e2e-{nanos}-{suffix}"))
}

fn write_fake_tmux(path: &Path) {
    fs::write(
        path,
        r#"#!/bin/sh
case "$1" in
  display-message)
    printf '%s\n' "$TMUX_E2E_CURRENT_SESSION"
    ;;
  list-sessions)
    cat "$TMUX_E2E_SESSIONS_FILE"
    ;;
  switch-client)
    while [ "$#" -gt 0 ]; do
      if [ "$1" = "-t" ]; then
        shift
        printf '%s\n' "$1" > "$TMUX_E2E_SWITCH_FILE"
        exit 0
      fi
      shift
    done
    exit 1
    ;;
  *)
    exit 1
    ;;
esac
"#,
    )
    .unwrap();

    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}

fn write_sessions(path: &Path, sessions: &[(&str, u64)]) {
    let contents = sessions
        .iter()
        .map(|(name, activity)| format!("{name}\t{activity}"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(path, format!("{contents}\n")).unwrap();
}

fn open_pty(rows: u16, cols: u16) -> (File, File) {
    let mut master_fd = MaybeUninit::<libc::c_int>::uninit();
    let mut slave_fd = MaybeUninit::<libc::c_int>::uninit();
    let winsize = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };

    let rc = unsafe {
        libc::openpty(
            master_fd.as_mut_ptr(),
            slave_fd.as_mut_ptr(),
            std::ptr::null_mut(),
            std::ptr::null(),
            &winsize,
        )
    };
    assert_eq!(rc, 0, "openpty failed: {}", io::Error::last_os_error());

    unsafe {
        (
            File::from_raw_fd(master_fd.assume_init()),
            File::from_raw_fd(slave_fd.assume_init()),
        )
    }
}

fn spawn_picker(
    temp_dir: &Path,
    sessions_file: &Path,
    current_session: &str,
    switch_file: &Path,
    pin_file: &Path,
    slave: File,
) -> Child {
    let stdin = Stdio::from(slave.try_clone().unwrap());
    let stdout = Stdio::from(slave.try_clone().unwrap());
    let stderr = Stdio::from(slave);
    let path = env::join_paths(
        std::iter::once(temp_dir.to_path_buf())
            .chain(env::split_paths(&env::var_os("PATH").unwrap())),
    )
    .unwrap();

    Command::new(env!("CARGO_BIN_EXE_tmux-session-picker"))
        .stdin(stdin)
        .stdout(stdout)
        .stderr(stderr)
        .env("PATH", path)
        .env("HOME", temp_dir)
        .env("TMUX_E2E_CURRENT_SESSION", current_session)
        .env("TMUX_E2E_SESSIONS_FILE", sessions_file)
        .env("TMUX_E2E_SWITCH_FILE", switch_file)
        .env("TMUX_SESSION_PIN_FILE", pin_file)
        .env("TMUX_SESSION_GROUP_FILE", temp_dir.join("groups.toml"))
        .spawn()
        .unwrap()
}

fn wait_for_output(master: &mut File, expected: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    let mut output = String::new();
    let mut buffer = [0_u8; 1024];

    while Instant::now() < deadline {
        if input_is_ready(master.as_raw_fd(), Duration::from_millis(50)) {
            match master.read(&mut buffer) {
                Ok(0) => {}
                Ok(size) => {
                    output.push_str(&String::from_utf8_lossy(&buffer[..size]));
                    if output.contains(expected) {
                        return;
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) => panic!("failed to read pty: {err}"),
            }
        }
    }

    panic!("timed out waiting for {expected:?}; output was {output:?}");
}

fn wait_for_file(path: &Path, timeout: Duration, master: &mut File, child: &mut Child) -> String {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(contents) = fs::read_to_string(path) {
            return contents;
        }
        thread::sleep(Duration::from_millis(20));
    }

    let mut output = String::new();
    let mut buffer = [0_u8; 4096];
    while input_is_ready(master.as_raw_fd(), Duration::from_millis(10)) {
        match master.read(&mut buffer) {
            Ok(0) => break,
            Ok(size) => output.push_str(&String::from_utf8_lossy(&buffer[..size])),
            Err(_) => break,
        }
    }
    let status = child.try_wait().unwrap();
    let _ = child.kill();
    panic!(
        "timed out waiting for {}; child status: {:?}; extra output: {:?}",
        path.display(),
        status,
        output
    );
}

fn wait_for_file_contents(
    path: &Path,
    expected: &str,
    timeout: Duration,
    master: &mut File,
    child: &mut Child,
) {
    let deadline = Instant::now() + timeout;
    let mut last_contents = String::new();
    while Instant::now() < deadline {
        if let Ok(contents) = fs::read_to_string(path) {
            if contents == expected {
                return;
            }
            last_contents = contents;
        }
        thread::sleep(Duration::from_millis(20));
    }

    let status = child.try_wait().unwrap();
    let mut output = String::new();
    let mut buffer = [0_u8; 4096];
    while input_is_ready(master.as_raw_fd(), Duration::from_millis(10)) {
        match master.read(&mut buffer) {
            Ok(0) => break,
            Ok(size) => output.push_str(&String::from_utf8_lossy(&buffer[..size])),
            Err(_) => break,
        }
    }
    let _ = child.kill();
    panic!(
        "timed out waiting for {} to become {:?}; last contents: {:?}; child status: {:?}; extra output: {:?}",
        path.display(),
        expected,
        last_contents,
        status,
        output
    );
}

fn input_is_ready(fd: libc::c_int, timeout: Duration) -> bool {
    let mut poll_fd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let timeout_ms = timeout.as_millis().try_into().unwrap_or(i32::MAX);
    let result = unsafe { libc::poll(&mut poll_fd, 1, timeout_ms) };
    assert!(result >= 0, "poll failed: {}", io::Error::last_os_error());
    result > 0 && poll_fd.revents & libc::POLLIN != 0
}
