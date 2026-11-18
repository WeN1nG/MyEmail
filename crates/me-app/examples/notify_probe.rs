//! 探针：**直接**调 notify-rust 发一条通知并打印真实结果。
//!
//! 不参与产品逻辑，只回答一个问题：这台机器上「从 Rust 发 Windows toast」
//! 这条路通不通。用来把「通知没弹」拆成两半——触发没走到，还是平台调用失败。
//!
//! ```bash
//! cargo run -p me-app --example notify_probe
//! ```

fn main() {
    let result = notify_rust::Notification::new()
        .summary("MyEmail 通知探针")
        .body("看到这条说明平台通道可用")
        .show();
    match result {
        Ok(handle) => println!("OK 平台调用成功：{handle:?}"),
        Err(err) => println!("ERR 平台调用失败：{err:?}"),
    }
}
