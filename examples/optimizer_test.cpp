#include "sealdb/optimizer.h"
#include "sealdb/parser.h"
#include "sealdb/logger.h"
#include <iostream>
#include <memory>

using namespace sealdb;

/**
 * @brief 测试成本估算器
 */
void test_cost_estimator() {
    std::cout << "\n=== Testing Cost Estimator ===" << std::endl;

    CostEstimator estimator;

    // 测试扫描成本
    double scan_cost = estimator.estimate_scan_cost("users");
    std::cout << "Scan cost for 'users' table: " << scan_cost << std::endl;

    // 测试连接成本
    double join_cost = estimator.estimate_join_cost("users", "orders", nullptr);
    std::cout << "Join cost for 'users' and 'orders': " << join_cost << std::endl;

    // 测试聚合成本
    std::vector<std::unique_ptr<Expression>> group_by;
    double agg_cost = estimator.estimate_aggregation_cost(group_by, nullptr);
    std::cout << "Aggregation cost: " << agg_cost << std::endl;

    // 测试排序成本
    std::vector<std::unique_ptr<Expression>> order_by;
    double sort_cost = estimator.estimate_sort_cost(order_by, 1000);
    std::cout << "Sort cost for 1000 rows: " << sort_cost << std::endl;

    // 测试过滤成本
    double filter_cost = estimator.estimate_filter_cost(nullptr, 1000);
    std::cout << "Filter cost for 1000 rows: " << filter_cost << std::endl;

    // 测试投影成本
    std::vector<std::unique_ptr<Expression>> select_list;
    double project_cost = estimator.estimate_projection_cost(select_list, 1000);
    std::cout << "Projection cost for 1000 rows: " << project_cost << std::endl;
}

/**
 * @brief 测试索引选择器
 */
void test_index_selector() {
    std::cout << "\n=== Testing Index Selector ===" << std::endl;

    IndexSelector selector;

    // 测试扫描索引选择
    std::string scan_index = selector.select_scan_index("users", nullptr);
    std::cout << "Selected scan index for 'users': " << (scan_index.empty() ? "none" : scan_index) << std::endl;

    // 测试连接索引选择
    std::string join_index = selector.select_join_index("users", "id");
    std::cout << "Selected join index for 'users.id': " << (join_index.empty() ? "none" : join_index) << std::endl;

    // 测试排序索引选择
    std::vector<std::string> order_columns = {"name", "age"};
    std::string sort_index = selector.select_sort_index("users", order_columns);
    std::cout << "Selected sort index for 'users': " << (sort_index.empty() ? "none" : sort_index) << std::endl;

    // 测试索引可用性
    bool is_usable = selector.is_index_usable("idx_users_name", nullptr);
    std::cout << "Index 'idx_users_name' usable: " << (is_usable ? "yes" : "no") << std::endl;
}

/**
 * @brief 测试执行计划生成器
 */
void test_planner() {
    std::cout << "\n=== Testing Planner ===" << std::endl;

    Planner planner;

    // 测试DROP TABLE语句（这个最简单）
    std::string sql = "DROP TABLE users";
    Parser parser(sql);
    auto statement = parser.parse();

    if (statement) {
        std::cout << "Successfully parsed: " << sql << std::endl;

        // 生成执行计划
        auto plan = planner.plan(std::move(statement));
        if (plan) {
            std::cout << "Generated execution plan:" << std::endl;
            std::cout << plan->to_string() << std::endl;
        } else {
            std::cout << "Failed to generate execution plan" << std::endl;
        }
    } else {
        std::cout << "Failed to parse: " << sql << std::endl;
    }
}

int main() {
    Logger::info("Starting Optimizer Test");

    // 测试执行计划生成器
    test_planner();

    // 测试成本估算器
    test_cost_estimator();

    // 测试索引选择器
    test_index_selector();

    Logger::info("Optimizer Test completed");

    return 0;
}