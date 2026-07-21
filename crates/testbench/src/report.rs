//! 結果集約 → 人間向け表＋--json 成果物＋終了コード。

use serde::Serialize;

#[derive(Serialize, Clone)]
pub struct ItemReport {
    pub item: u32,
    pub name: String,
    pub status: String, // "pass" | "fail" | "error"
    pub detail: String,
    pub max_elapsed_ms: u128,
}

#[derive(Serialize)]
pub struct HarnessReport {
    pub all_pass: bool,
    pub started: bool,       // TsfHost::start が成功したか（false=実行不能）
    pub start_error: Option<String>,
    pub items: Vec<ItemReport>,
}

impl HarnessReport {
    pub fn print_table(&self) {
        println!("\n  # | result | item / detail");
        println!("  --+--------+-----------------------------");
        if !self.started {
            println!("  ! | ERROR  | TsfHost 起動失敗: {}", self.start_error.clone().unwrap_or_default());
        }
        for it in &self.items {
            let mark = match it.status.as_str() { "pass" => "✅ PASS", "fail" => "❌ FAIL", _ => "⚠ ERROR" };
            println!("  {} | {} | {} — {}", it.item, mark, it.name, it.detail);
        }
        println!();
    }
    /// 終了コード: 全 PASS かつ起動成功で 0、FAIL で 1、起動不能/エラーで 2。
    pub fn exit_code(&self) -> i32 {
        if !self.started { return 2; }
        if self.items.iter().any(|i| i.status == "error") { return 2; }
        if self.all_pass { 0 } else { 1 }
    }
    pub fn write_json(&self, path: &str) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(self).unwrap();
        std::fs::write(path, json)
    }
}
