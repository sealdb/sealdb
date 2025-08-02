//! SQL 解析器 Debug 日志示例
//!
//! 展示如何查看SQL解析器输出的AST抽象语法树

use sql::parser::SqlParser;
use tracing_subscriber;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 初始化日志系统
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .init();

    println!("=== SQL 解析器 Debug 日志示例 ===");

    let parser = SqlParser::new();

    // 测试不同类型的SQL语句
    let test_sqls = vec![
        "SELECT * FROM users WHERE id = 1",
        "INSERT INTO users (name, age) VALUES ('Alice', 25)",
        "UPDATE users SET age = 26 WHERE id = 1",
        "DELETE FROM users WHERE id = 1",
        "CREATE TABLE users (id INT, name VARCHAR(255))",
        "DROP TABLE users",
        "INVALID SQL STATEMENT", // 这个应该失败
    ];

    for (i, sql) in test_sqls.iter().enumerate() {
        println!("\n--- 测试 SQL {} ---", i + 1);
        println!("输入SQL: {}", sql);

        match parser.parse(sql) {
            Ok(ast) => {
                println!("✓ 解析成功");
                println!("AST类型: {:?}", std::mem::discriminant(&ast));
                println!("AST内容: {:#?}", ast);
            }
            Err(e) => {
                println!("✗ 解析失败: {}", e);
            }
        }
    }

    println!("\n=== 示例完成 ===");
    Ok(())
}