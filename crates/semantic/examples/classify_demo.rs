// SPDX-License-Identifier: Apache-2.0
//! Demo: run Heddle's change classification on realistic file pairs.
//!
//! ```sh
//! cargo run -p semantic --example classify_demo
//! ```

use std::path::Path;

use semantic::classify_modification;

fn main() {
    let pairs: Vec<(&str, &str, &str, &str)> = vec![
        // 1. Rust: renamed function, added retry logic, new imports — LOGIC
        (
            "main.rs",
            concat!(
                "use std::collections::HashMap;\n",
                "use std::io;\n\n",
                "// Main entry point\n",
                "fn main() {\n",
                "    let config = load_config();\n",
                "    let result = process(&config);\n",
                "    println!(\"Result: {}\", result);\n",
                "}\n\n",
                "fn load_config() -> HashMap<String, String> {\n",
                "    let mut map = HashMap::new();\n",
                "    map.insert(\"key\".to_string(), \"value\".to_string());\n",
                "    map\n",
                "}\n\n",
                "fn process(config: &HashMap<String, String>) -> String {\n",
                "    let key = config.get(\"key\").unwrap_or(&String::new());\n",
                "    format!(\"Processed: {}\", key)\n",
                "}\n",
            ),
            concat!(
                "use std::collections::HashMap;\n",
                "use std::io;\n",
                "use std::thread;\n",
                "use std::time::Duration;\n\n",
                "// Main entry point — now with retry support\n",
                "fn main() {\n",
                "    let config = load_config();\n",
                "    let result = process_with_retry(&config, 3);\n",
                "    println!(\"Result: {}\", result);\n",
                "}\n\n",
                "fn load_config() -> HashMap<String, String> {\n",
                "    let mut map = HashMap::new();\n",
                "    map.insert(\"key\".to_string(), \"value\".to_string());\n",
                "    map\n",
                "}\n\n",
                "fn process_with_retry(config: &HashMap<String, String>, max_retries: u32) -> String {\n",
                "    let mut attempts = 0;\n",
                "    loop {\n",
                "        match try_process(config) {\n",
                "            Ok(result) => return result,\n",
                "            Err(e) if attempts < max_retries => {\n",
                "                attempts += 1;\n",
                "                eprintln!(\"Retry {}/{}: {}\", attempts, max_retries, e);\n",
                "                thread::sleep(Duration::from_millis(100 * attempts as u64));\n",
                "            }\n",
                "            Err(e) => return format!(\"Failed after {} retries: {}\", max_retries, e),\n",
                "        }\n",
                "    }\n",
                "}\n\n",
                "fn try_process(config: &HashMap<String, String>) -> Result<String, String> {\n",
                "    let key = config.get(\"key\").ok_or(\"missing key\")?;\n",
                "    Ok(format!(\"Processed: {}\", key))\n",
                "}\n",
            ),
            "Logic: renamed fn + retry logic + new imports",
        ),
        // 2. Rust: formatting only (indentation added) — FORMATTING
        (
            "utils.rs",
            "fn helper_one() {\nlet x = 1;\nlet y = 2;\nx + y\n}\n\nfn helper_two(a: i32, b: i32) -> i32 {\na * b + 1\n}\n\nfn helper_three() -> String {\n\"hello world\".to_string()\n}\n",
            "fn helper_one() {\n    let x = 1;\n    let y = 2;\n    x + y\n}\n\nfn helper_two(a: i32, b: i32) -> i32 {\n    a * b + 1\n}\n\nfn helper_three() -> String {\n    \"hello world\".to_string()\n}\n",
            "Noise: formatting only (indentation)",
        ),
        // 3. Rust: only comments changed — COMMENTS
        (
            "config.rs",
            "// Configuration module\n// Handles loading and parsing config files\nuse std::path::Path;\n\nfn read_config(path: &Path) -> String {\n    std::fs::read_to_string(path).unwrap_or_default()\n}\n",
            "// Configuration module — refactored for clarity\n// Handles loading, parsing, and validating config files\n// See DESIGN.md for the full config spec\nuse std::path::Path;\n\nfn read_config(path: &Path) -> String {\n    std::fs::read_to_string(path).unwrap_or_default()\n}\n",
            "Low: comments only",
        ),
        // 4. Rust: only imports changed — IMPORTS
        (
            "imports.rs",
            "use std::io;\n\nfn do_work() -> i32 {\n    42\n}\n",
            "use std::io;\nuse std::fs;\nuse std::path::PathBuf;\n\nfn do_work() -> i32 {\n    42\n}\n",
            "Low: imports only",
        ),
        // 5. Python: formatting only (spaces around operators) — FORMATTING
        (
            "script.py",
            "def calculate(x,y):\n    return x+y\n\ndef transform(data):\n    result=[]\n    for item in data:\n        result.append(item*2)\n    return result\n",
            "def calculate(x, y):\n    return x + y\n\ndef transform(data):\n    result = []\n    for item in data:\n        result.append(item * 2)\n    return result\n",
            "Noise: Python formatting (spaces around ops)",
        ),
        // 6. TypeScript: function deleted + new function + logic — LOGIC
        (
            "handler.ts",
            concat!(
                "function handleRequest(req: Request): Response {\n",
                "    const body = parseBody(req);\n",
                "    return new Response(JSON.stringify(body));\n",
                "}\n\n",
                "function parseBody(req: Request): any {\n",
                "    return JSON.parse(req.body as string);\n",
                "}\n\n",
                "function legacyHandler(data: string): string {\n",
                "    return data.toUpperCase();\n",
                "}\n",
            ),
            concat!(
                "function handleRequest(req: Request): Response {\n",
                "    const body = parseBody(req);\n",
                "    const validated = validateBody(body);\n",
                "    return new Response(JSON.stringify(validated));\n",
                "}\n\n",
                "function parseBody(req: Request): any {\n",
                "    return JSON.parse(req.body as string);\n",
                "}\n\n",
                "function validateBody(body: any): any {\n",
                "    if (!body || typeof body !== 'object') {\n",
                "        throw new Error('Invalid body');\n",
                "    }\n",
                "    return body;\n",
                "}\n",
            ),
            "Logic: fn deleted + new fn + logic change",
        ),
        // 7. Go: pure logic change — LOGIC
        (
            "server.go",
            "package main\n\nimport \"fmt\"\n\nfunc handleHealth() string {\n\treturn \"ok\"\n}\n\nfunc handleMetrics() string {\n\treturn fmt.Sprintf(\"uptime=%d\", 42)\n}\n",
            "package main\n\nimport \"fmt\"\n\nfunc handleHealth() string {\n\treturn \"healthy\"\n}\n\nfunc handleMetrics() string {\n\treturn fmt.Sprintf(\"uptime=%d,errors=%d\", 42, 0)\n}\n",
            "Logic: Go return value changes",
        ),
        // 8. Java: formatting only (brace style) — FORMATTING
        (
            "App.java",
            "class App {\npublic static void main(String[] args) {\nSystem.out.println(\"hello\");\n}\n}\n",
            "class App {\n    public static void main(String[] args) {\n        System.out.println(\"hello\");\n    }\n}\n",
            "Noise: Java formatting (brace indentation)",
        ),
        // 9. Unknown file type: TOML config — token-level fallback
        (
            "config.toml",
            "[database]\nhost = \"localhost\"\nport = 5432\n",
            "[database]\nhost = \"db.prod.internal\"\nport = 5432\ntimeout = 30\n",
            "Logic: TOML config value change (token fallback)",
        ),
    ];

    println!();
    println!("  Heddle Semantic Diff — Classification Demo");
    println!("  =========================================");
    println!();
    println!(
        "  {:<14} {:<18} {:<10} Expected",
        "File", "Classification", "Importance"
    );
    println!("  {}", "─".repeat(78));

    let mut noise_count = 0;
    let mut low_count = 0;
    let mut logic_count = 0;

    for (name, old, new, expected) in &pairs {
        let (kind, importance) = classify_modification(Path::new(name), old, new);
        let imp_str = format!("{:?}", importance);
        println!(
            "  {:<14} {:<18} {:<10} {}",
            name,
            format!("{:?}", kind),
            imp_str,
            expected
        );
        match importance {
            objects::object::ChangeImportance::Noise => noise_count += 1,
            objects::object::ChangeImportance::Low => low_count += 1,
            _ => logic_count += 1,
        }
    }

    println!("  {}", "─".repeat(78));
    println!();
    println!(
        "  {} files changed → {} things worth reviewing",
        pairs.len(),
        logic_count
    );
    println!(
        "  Filtered: {} noise (formatting) + {} low (imports/comments)",
        noise_count, low_count
    );
    println!();
}
