//! 探针 v2：分两种情况定位「输入进不去」
//!  A) python（不做光标查询）→ 检验 PTY 写入通道本身
//!  B) cmd（启动即发 ESC[6n 光标查询）→ 检验是否因未应答 CPR 而卡死
//! 运行：`cargo run --example pty_probe`
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

struct Case {
    label: &'static str,
    program: String,
    args: Vec<String>,
    /// 看到 CPR 请求时是否应答（模拟 xterm）
    answer_cpr: bool,
    payload: &'static str,
}

fn run(case: Case) {
    println!("\n===== {} =====", case.label);
    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows: 24,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");

    let mut cmd = CommandBuilder::new(&case.program);
    for a in &case.args {
        cmd.arg(a);
    }
    let _child = pair.slave.spawn_command(cmd).expect("spawn");
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().expect("reader");
    let writer = pair.master.take_writer().expect("writer");

    let captured = Arc::new(Mutex::new(Vec::<u8>::new()));
    let sink = captured.clone();
    let answer = case.answer_cpr;
    let writer_shared = Arc::new(Mutex::new(writer));
    let writer_for_reader = writer_shared.clone();

    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    sink.lock().unwrap().extend_from_slice(&buf[..n]);
                    if answer && buf[..n].windows(4).any(|w| w == b"\x1b[6n") {
                        let mut w = writer_for_reader.lock().unwrap();
                        let _ = w.write_all(b"\x1b[24;1R"); // 模拟光标位置应答
                        let _ = w.flush();
                    }
                }
            }
        }
    });

    std::thread::sleep(std::time::Duration::from_millis(900));

    {
        let mut w = writer_shared.lock().unwrap();
        let _ = w.write_all(case.payload.as_bytes());
        let _ = w.flush();
    }

    std::thread::sleep(std::time::Duration::from_millis(1800));

    let bytes = captured.lock().unwrap().clone();
    let text = String::from_utf8_lossy(&bytes);
    println!("--- 输出 {} 字节 ---", bytes.len());
    println!("{:?}", text);
    let ok = text.contains("YH-INPUT-OK") || text.contains("GOT:");
    println!(
        "判定: {}",
        if ok {
            "PASS —— 输入到达子进程"
        } else if text.contains("\x1b[6n") {
            "BLOCKED —— 子进程在等 ESC[6n 应答，写入被它忽略"
        } else {
            "FAIL —— 字节没进去"
        }
    );
}

fn main() {
    let python = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "python".to_string());

    run(Case {
        label: "A) python（不做光标查询）",
        program: python,
        args: vec![
            "-c".into(),
            "import sys; sys.stdout.write('READY\\n'); sys.stdout.flush(); sys.stdin.readline(); print('GOT:', 'ok')".into(),
        ],
        answer_cpr: true,
        payload: "anything\r",
    });

    run(Case {
        label: "B) cmd（不应答 CPR）",
        program: "cmd.exe".into(),
        args: vec!["/Q".into()],
        answer_cpr: false,
        payload: "echo YH-INPUT-OK\r",
    });

    run(Case {
        label: "C) cmd（应答 CPR，模拟 xterm）",
        program: "cmd.exe".into(),
        args: vec!["/Q".into()],
        answer_cpr: true,
        payload: "echo YH-INPUT-OK\r",
    });

    std::process::exit(0);
}
