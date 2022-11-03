-- 测试名称: 基本 SELECT 查询
-- 描述: 验证基本的 SELECT 语句功能
-- 标签: basic, select, regression
-- 超时: 30秒

-- 准备测试数据
CREATE TABLE IF NOT EXISTS users (
    id INT PRIMARY KEY,
    name VARCHAR(50) NOT NULL,
    age INT,
    email VARCHAR(100)
);

-- 清理旧数据
DELETE FROM users;

-- 插入测试数据
INSERT INTO users (id, name, age, email) VALUES
(1, 'Alice', 25, 'alice@example.com'),
(2, 'Bob', 30, 'bob@example.com'),
(3, 'Charlie', 35, 'charlie@example.com'),
(4, 'Diana', 28, 'diana@example.com'),
(5, 'Eve', 32, 'eve@example.com');

-- 测试用例 1: 基本 SELECT
-- 期望结果: 返回所有用户数据
SELECT * FROM users ORDER BY id;

-- 期望结果:
-- id | name   | age | email
-- 1  | Alice  | 25  | alice@example.com
-- 2  | Bob    | 30  | bob@example.com
-- 3  | Charlie| 35  | charlie@example.com
-- 4  | Diana  | 28  | diana@example.com
-- 5  | Eve    | 32  | eve@example.com

-- 测试用例 2: 带条件的 SELECT
-- 期望结果: 返回年龄大于 30 的用户
SELECT id, name, age FROM users WHERE age > 30 ORDER BY id;

-- 期望结果:
-- id | name   | age
-- 3  | Charlie| 35
-- 5  | Eve    | 32

-- 测试用例 3: 聚合查询
-- 期望结果: 计算平均年龄
SELECT AVG(age) as average_age FROM users;

-- 期望结果:
-- average_age
-- 30.0

-- 测试用例 4: 分组查询
-- 期望结果: 按年龄分组统计用户数量
SELECT age, COUNT(*) as user_count FROM users GROUP BY age ORDER BY age;

-- 期望结果:
-- age | user_count
-- 25  | 1
-- 28  | 1
-- 30  | 1
-- 32  | 1
-- 35  | 1

-- 测试用例 5: 排序查询
-- 期望结果: 按年龄降序排列
SELECT name, age FROM users ORDER BY age DESC;

-- 期望结果:
-- name   | age
-- Charlie| 35
-- Eve    | 32
-- Bob    | 30
-- Diana  | 28
-- Alice  | 25

-- 测试用例 6: LIMIT 查询
-- 期望结果: 返回前 3 个用户
SELECT id, name FROM users ORDER BY id LIMIT 3;

-- 期望结果:
-- id | name
-- 1  | Alice
-- 2  | Bob
-- 3  | Charlie

-- 测试用例 7: 列别名
-- 期望结果: 使用列别名
SELECT id as user_id, name as user_name FROM users WHERE id = 1;

-- 期望结果:
-- user_id | user_name
-- 1       | Alice

-- 测试用例 8: 字符串函数
-- 期望结果: 使用字符串函数
SELECT id, UPPER(name) as upper_name, LENGTH(name) as name_length FROM users WHERE id = 2;

-- 期望结果:
-- id | upper_name | name_length
-- 2  | BOB        | 3

-- 测试用例 9: 数值计算
-- 期望结果: 数值计算
SELECT id, name, age, age + 1 as next_year_age FROM users WHERE id = 3;

-- 期望结果:
-- id | name   | age | next_year_age
-- 3  | Charlie| 35  | 36

-- 测试用例 10: 空值处理
-- 期望结果: 处理空值
INSERT INTO users (id, name, age, email) VALUES (6, 'Frank', NULL, NULL);
SELECT id, name, age, email FROM users WHERE id = 6;

-- 期望结果:
-- id | name | age | email
-- 6  | Frank| NULL| NULL

-- 清理测试数据
DROP TABLE users;