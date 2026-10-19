use std::process::Command;
use std::fs;
use tempfile::TempDir;

/// 命令行参数集成测试
pub struct CliIntegrationTest {
    temp_dir: TempDir,
}

impl CliIntegrationTest {
    pub fn new() -> Self {
        let temp_dir = TempDir::new().expect("Failed to create temp directory");
        Self { temp_dir }
    }

        /// 运行 sealdb 命令并返回输出
    fn run_sealdb(&self, args: &[&str]) -> (String, String, i32) {
        println!("Running command: cargo run --bin sealdb -- {:?}", args);
        let output = Command::new("cargo")
            .args(&["run", "--bin", "sealdb", "--"])
            .args(args)
            .current_dir("/home/wslu/work/github/sealdb")
            .output();

        match output {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                let exit_code = output.status.code().unwrap_or(-1);
                println!("Exit code: {}, Stdout: {}, Stderr: {}", exit_code, stdout, stderr);
                (stdout, stderr, exit_code)
            }
            Err(e) => {
                println!("Command failed: {:?}", e);
                (String::new(), e.to_string(), -1)
            }
        }
    }

    /// 测试帮助信息
    pub fn test_help(&self) -> bool {
        println!("Testing help command...");

        let (stdout, stderr, exit_code) = self.run_sealdb(&["--help"]);

        if exit_code != 0 {
            println!("Help command failed with exit code: {}", exit_code);
            return false;
        }

        let expected_keywords = [
            "SealDB - 高性能分布式数据库",
            "Usage: sealdb [OPTIONS]",
            "--help",
            "--version",
        ];

        for keyword in &expected_keywords {
            if !stdout.contains(keyword) {
                println!("Help output missing keyword: {}", keyword);
                return false;
            }
        }

        println!("✓ Help command test passed");
        true
    }

    /// 测试版本信息
    pub fn test_version(&self) -> bool {
        println!("Testing version command...");

        let (stdout, stderr, exit_code) = self.run_sealdb(&["--version"]);

        if exit_code != 0 {
            println!("Version command failed with exit code: {}", exit_code);
            return false;
        }

        if !stdout.contains("sealdb") || !stdout.contains("0.1.0") {
            println!("Version output format incorrect: {}", stdout);
            return false;
        }

        println!("✓ Version command test passed");
        true
    }

        /// 测试生成配置文件
    pub fn test_generate_config(&self) -> bool {
        println!("Testing generate config command...");

        let (stdout, stderr, exit_code) = self.run_sealdb(&["--generate-config"]);

        if exit_code != 0 {
            println!("Generate config command failed with exit code: {}", exit_code);
            return false;
        }

        if !stdout.contains("默认配置文件已生成") {
            println!("Generate config output missing expected message: {}", stdout);
            return false;
        }

        // 检查配置文件是否在项目根目录中创建
        let config_files = ["config.toml", "config.json", "config.yaml"];
        for file in &config_files {
            let file_path = std::path::Path::new("/home/wslu/work/github/sealdb").join(file);
            if !file_path.exists() {
                println!("Config file not created: {}", file);
                return false;
            }

            // 验证文件内容不为空
            if let Ok(content) = std::fs::read_to_string(&file_path) {
                if content.is_empty() {
                    println!("Config file is empty: {}", file);
                    return false;
                }
            } else {
                println!("Failed to read config file: {}", file);
                return false;
            }
        }

        println!("✓ Generate config command test passed");
        true
    }

    /// 测试显示配置
    pub fn test_show_config(&self) -> bool {
        println!("Testing show config command...");

        let (stdout, stderr, exit_code) = self.run_sealdb(&["--show-config"]);

        if exit_code != 0 {
            println!("Show config command failed with exit code: {}", exit_code);
            return false;
        }

        let expected_config_keywords = [
            "Config {",
            "server: ServerConfig {",
            "host: \"0.0.0.0\"",
            "port: 4000",
        ];

        for keyword in &expected_config_keywords {
            if !stdout.contains(keyword) {
                println!("Show config output missing keyword: {}", keyword);
                return false;
            }
        }

        println!("✓ Show config command test passed");
        true
    }

    /// 测试命令行参数覆盖
    pub fn test_cli_overrides(&self) -> bool {
        println!("Testing CLI parameter overrides...");

        let (stdout, stderr, exit_code) = self.run_sealdb(&[
            "--host", "192.168.1.100",
            "--port", "8080",
            "--log-level", "debug",
            "--show-config"
        ]);

        if exit_code != 0 {
            println!("CLI overrides test failed with exit code: {}", exit_code);
            return false;
        }

        if !stdout.contains("host: \"192.168.1.100\"") {
            println!("Host override not applied: {}", stdout);
            return false;
        }

        if !stdout.contains("port: 8080") {
            println!("Port override not applied: {}", stdout);
            return false;
        }

        println!("✓ CLI overrides test passed");
        true
    }

    /// 运行所有测试
    pub fn run_all_tests(&self) -> bool {
        println!("Running CLI integration tests...");
        println!("==================================");

        let tests = vec![
            ("Help", self.test_help()),
            ("Version", self.test_version()),
            ("Generate Config", self.test_generate_config()),
            ("Show Config", self.test_show_config()),
            ("CLI Overrides", self.test_cli_overrides()),
        ];

        let mut passed = 0;
        let mut failed = 0;

        for (test_name, result) in tests {
            if result {
                passed += 1;
            } else {
                failed += 1;
                println!("✗ {} test failed", test_name);
            }
        }

        println!("==================================");
        println!("Test Results:");
        println!("Passed: {}", passed);
        println!("Failed: {}", failed);
        println!("Total: {}", passed + failed);

        failed == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cli_integration() {
        let test = CliIntegrationTest::new();
        assert!(test.run_all_tests());
    }
}