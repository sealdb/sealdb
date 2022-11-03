#include "sealdb/optimizer.h"
#include "sealdb/planner.h"
#include "sealdb/logger.h"
#include "sealdb/ast.h"
#include <algorithm>
#include <cmath>

namespace sealdb {

// Optimizer 实现
Optimizer::Optimizer() {
    Logger::info("Optimizer initialized");
}

std::unique_ptr<ExecutionPlan> Optimizer::optimize(std::unique_ptr<Statement> statement) {
    if (!statement) {
        Logger::error("Cannot optimize null statement");
        return nullptr;
    }

    Logger::info("Starting query optimization");

    // 根据语句类型进行优化
    if (auto select_stmt = dynamic_cast<SelectStatement*>(statement.get())) {
        return optimize_select(select_stmt);
    } else if (auto insert_stmt = dynamic_cast<InsertStatement*>(statement.get())) {
        return optimize_insert(insert_stmt);
    } else if (auto update_stmt = dynamic_cast<UpdateStatement*>(statement.get())) {
        return optimize_update(update_stmt);
    } else if (auto delete_stmt = dynamic_cast<DeleteStatement*>(statement.get())) {
        return optimize_delete(delete_stmt);
    } else if (auto create_table_stmt = dynamic_cast<CreateTableStatement*>(statement.get())) {
        return optimize_create_table(create_table_stmt);
    } else if (auto drop_table_stmt = dynamic_cast<DropTableStatement*>(statement.get())) {
        return optimize_drop_table(drop_table_stmt);
    } else {
        Logger::error("Unknown statement type in optimizer");
        return nullptr;
    }
}

std::unique_ptr<ExecutionPlan> Optimizer::optimize_select(SelectStatement* stmt) {
    if (!stmt) return nullptr;

    Logger::info("Optimizing SELECT statement");

    // 生成初始执行计划
    Planner planner;
    auto plan = planner.plan_select(stmt);
    if (!plan) {
        Logger::error("Failed to generate initial plan for SELECT");
        return nullptr;
    }

    // 应用优化策略
    plan = apply_predicate_pushdown(std::move(plan));
    plan = apply_index_selection(std::move(plan));
    plan = apply_column_pruning(std::move(plan));
    plan = apply_subquery_optimization(std::move(plan));

    Logger::info("SELECT optimization completed");
    return plan;
}

std::unique_ptr<ExecutionPlan> Optimizer::optimize_insert(InsertStatement* stmt) {
    if (!stmt) return nullptr;

    Logger::info("Optimizing INSERT statement");

    // INSERT语句优化相对简单
    Planner planner;
    auto plan = planner.plan_insert(stmt);

    Logger::info("INSERT optimization completed");
    return plan;
}

std::unique_ptr<ExecutionPlan> Optimizer::optimize_update(UpdateStatement* stmt) {
    if (!stmt) return nullptr;

    Logger::info("Optimizing UPDATE statement");

    // 生成初始执行计划
    Planner planner;
    auto plan = planner.plan_update(stmt);
    if (!plan) {
        Logger::error("Failed to generate initial plan for UPDATE");
        return nullptr;
    }

    // 应用优化策略
    plan = apply_predicate_pushdown(std::move(plan));
    plan = apply_index_selection(std::move(plan));

    Logger::info("UPDATE optimization completed");
    return plan;
}

std::unique_ptr<ExecutionPlan> Optimizer::optimize_delete(DeleteStatement* stmt) {
    if (!stmt) return nullptr;

    Logger::info("Optimizing DELETE statement");

    // 生成初始执行计划
    Planner planner;
    auto plan = planner.plan_delete(stmt);
    if (!plan) {
        Logger::error("Failed to generate initial plan for DELETE");
        return nullptr;
    }

    // 应用优化策略
    plan = apply_predicate_pushdown(std::move(plan));
    plan = apply_index_selection(std::move(plan));

    Logger::info("DELETE optimization completed");
    return plan;
}

std::unique_ptr<ExecutionPlan> Optimizer::optimize_create_table(CreateTableStatement* stmt) {
    if (!stmt) return nullptr;

    Logger::info("Optimizing CREATE TABLE statement");

    // CREATE TABLE语句优化相对简单
    Planner planner;
    auto plan = planner.plan_create_table(stmt);

    Logger::info("CREATE TABLE optimization completed");
    return plan;
}

std::unique_ptr<ExecutionPlan> Optimizer::optimize_drop_table(DropTableStatement* stmt) {
    if (!stmt) return nullptr;

    Logger::info("Optimizing DROP TABLE statement");

    // DROP TABLE语句优化相对简单
    Planner planner;
    auto plan = planner.plan_drop_table(stmt);

    Logger::info("DROP TABLE optimization completed");
    return plan;
}

// 优化策略实现
std::unique_ptr<ExecutionPlan> Optimizer::apply_predicate_pushdown(std::unique_ptr<ExecutionPlan> plan) {
    if (!plan || !plan->get_root()) return plan;

    Logger::info("Applying predicate pushdown optimization");

    // 这里实现谓词下推优化
    // 将过滤条件尽可能下推到扫描节点附近
    // 简化实现：暂时返回原计划
    return plan;
}

std::unique_ptr<ExecutionPlan> Optimizer::apply_join_reordering(std::unique_ptr<ExecutionPlan> plan) {
    if (!plan || !plan->get_root()) return plan;

    Logger::info("Applying join reordering optimization");

    // 这里实现连接重排序优化
    // 根据成本估算重新排列连接顺序
    // 简化实现：暂时返回原计划
    return plan;
}

std::unique_ptr<ExecutionPlan> Optimizer::apply_index_selection(std::unique_ptr<ExecutionPlan> plan) {
    if (!plan || !plan->get_root()) return plan;

    Logger::info("Applying index selection optimization");

    // 这里实现索引选择优化
    // 为扫描操作选择最优的索引
    // 简化实现：暂时返回原计划
    return plan;
}

std::unique_ptr<ExecutionPlan> Optimizer::apply_column_pruning(std::unique_ptr<ExecutionPlan> plan) {
    if (!plan || !plan->get_root()) return plan;

    Logger::info("Applying column pruning optimization");

    // 这里实现列裁剪优化
    // 只选择查询需要的列，减少数据传输
    // 简化实现：暂时返回原计划
    return plan;
}

std::unique_ptr<ExecutionPlan> Optimizer::apply_subquery_optimization(std::unique_ptr<ExecutionPlan> plan) {
    if (!plan || !plan->get_root()) return plan;

    Logger::info("Applying subquery optimization");

    // 这里实现子查询优化
    // 将子查询转换为连接或其他更高效的形式
    // 简化实现：暂时返回原计划
    return plan;
}

// 辅助方法实现
std::vector<std::string> Optimizer::extract_table_names(SelectStatement* stmt) {
    if (!stmt) return {};
    return stmt->get_from_tables();
}

std::vector<std::string> Optimizer::extract_column_names(SelectStatement* stmt) {
    if (!stmt) return {};

    std::vector<std::string> column_names;
    for (const auto& expr : stmt->get_select_list()) {
        if (auto col_ref = dynamic_cast<ColumnReference*>(expr.get())) {
            column_names.push_back(col_ref->get_column_name());
        }
    }
    return column_names;
}

    std::unique_ptr<Expression> Optimizer::extract_where_condition(SelectStatement* stmt) {
        if (!stmt || !stmt->get_where_clause()) return nullptr;
        return std::unique_ptr<Expression>(stmt->get_where_clause()->clone());
    }

    std::vector<std::unique_ptr<Expression>> Optimizer::extract_group_by(SelectStatement* stmt) {
        if (!stmt) return {};
        std::vector<std::unique_ptr<Expression>> group_by;
        for (const auto& expr : stmt->get_group_by()) {
            group_by.push_back(expr->clone());
        }
        return group_by;
    }

    std::unique_ptr<Expression> Optimizer::extract_having_condition(SelectStatement* stmt) {
        if (!stmt || !stmt->get_having_clause()) return nullptr;
        return std::unique_ptr<Expression>(stmt->get_having_clause()->clone());
    }

    std::vector<std::unique_ptr<Expression>> Optimizer::extract_order_by(SelectStatement* stmt) {
        if (!stmt) return {};
        std::vector<std::unique_ptr<Expression>> order_by;
        for (const auto& expr : stmt->get_order_by()) {
            order_by.push_back(expr->clone());
        }
        return order_by;
    }

// CostEstimator 实现
CostEstimator::CostEstimator() {
    Logger::info("CostEstimator initialized");
}

double CostEstimator::estimate_scan_cost(const std::string& table_name,
                                        const std::unique_ptr<Expression>& condition) {
    // 基础扫描成本
    double base_cost = 100.0;

    // 如果有条件，根据选择性调整成本
    if (condition) {
        double selectivity = estimate_selectivity(condition);
        base_cost *= selectivity;
    }

    return base_cost;
}

double CostEstimator::estimate_join_cost(const std::string& left_table,
                                        const std::string& right_table,
                                        const std::unique_ptr<Expression>& join_condition) {
    // 连接成本估算
    double left_cost = estimate_scan_cost(left_table);
    double right_cost = estimate_scan_cost(right_table);

    // 连接成本 = 左表成本 + 右表成本 + 连接操作成本
    double join_operation_cost = 200.0;

    return left_cost + right_cost + join_operation_cost;
}

double CostEstimator::estimate_aggregation_cost(const std::vector<std::unique_ptr<Expression>>& group_by,
                                               const std::unique_ptr<Expression>& having) {
    // 聚合成本估算
    double base_cost = 150.0;

    // 根据分组列数调整成本
    base_cost += group_by.size() * 10.0;

    // 如果有HAVING条件，增加成本
    if (having) {
        base_cost += 50.0;
    }

    return base_cost;
}

double CostEstimator::estimate_sort_cost(const std::vector<std::unique_ptr<Expression>>& order_by,
                                        size_t estimated_rows) {
    // 排序成本估算
    double base_cost = 300.0;

    // 根据排序列数和行数调整成本
    base_cost += order_by.size() * 20.0;
    base_cost += estimated_rows * 0.1;

    return base_cost;
}

double CostEstimator::estimate_filter_cost(const std::unique_ptr<Expression>& condition,
                                          size_t input_rows) {
    // 过滤成本估算
    double base_cost = 50.0;

    // 根据输入行数调整成本
    base_cost += input_rows * 0.05;

    return base_cost;
}

double CostEstimator::estimate_projection_cost(const std::vector<std::unique_ptr<Expression>>& select_list,
                                              size_t input_rows) {
    // 投影成本估算
    double base_cost = 10.0;

    // 根据投影列数调整成本
    base_cost += select_list.size() * 2.0;

    return base_cost;
}

double CostEstimator::estimate_selectivity(const std::unique_ptr<Expression>& condition) {
    // 简化实现：返回默认选择性
    return 0.1; // 10%的选择性
}

size_t CostEstimator::estimate_cardinality(const std::string& table_name,
                                          const std::unique_ptr<Expression>& condition) {
    // 简化实现：返回默认基数
    return 1000;
}

// IndexSelector 实现
IndexSelector::IndexSelector() {
    Logger::info("IndexSelector initialized");
}

std::string IndexSelector::select_scan_index(const std::string& table_name,
                                            const std::unique_ptr<Expression>& condition) {
    // 简化实现：返回空字符串表示不使用索引
    return "";
}

std::string IndexSelector::select_join_index(const std::string& table_name,
                                            const std::string& join_column) {
    // 简化实现：返回空字符串表示不使用索引
    return "";
}

std::string IndexSelector::select_sort_index(const std::string& table_name,
                                            const std::vector<std::string>& order_columns) {
    // 简化实现：返回空字符串表示不使用索引
    return "";
}

bool IndexSelector::is_index_usable(const std::string& index_name,
                                   const std::unique_ptr<Expression>& condition) {
    // 简化实现：暂时返回false
    return false;
}

double IndexSelector::calculate_index_selectivity(const IndexInfo& index,
                                                const std::unique_ptr<Expression>& condition) {
    // 简化实现：返回默认选择性
    return 0.1;
}

bool IndexSelector::matches_index_columns(const IndexInfo& index,
                                         const std::vector<std::string>& columns) {
    // 简化实现：暂时返回false
    return false;
}

} // namespace sealdb
