#include "sealdb/planner.h"
#include "sealdb/logger.h"
#include <iostream>
#include <sstream>

namespace sealdb {

// PlanNode to_string 实现
std::string ScanNode::to_string() const {
    std::ostringstream oss;
    oss << "Scan(" << table_name_ << ")";
    return oss.str();
}

std::string IndexScanNode::to_string() const {
    std::ostringstream oss;
    oss << "IndexScan(" << table_name_ << ", " << index_name_ << ")";
    return oss.str();
}

std::string FilterNode::to_string() const {
    std::ostringstream oss;
    oss << "Filter(" << (condition_ ? "condition" : "null") << ")";
    return oss.str();
}

std::string ProjectNode::to_string() const {
    std::ostringstream oss;
    oss << "Project(" << expressions_.size() << " columns)";
    return oss.str();
}

std::string JoinNode::to_string() const {
    std::ostringstream oss;
    std::string join_type_str;
    switch (join_type_) {
        case JoinType::INNER: join_type_str = "INNER"; break;
        case JoinType::LEFT: join_type_str = "LEFT"; break;
        case JoinType::RIGHT: join_type_str = "RIGHT"; break;
        case JoinType::FULL: join_type_str = "FULL"; break;
    }
    oss << "Join(" << join_type_str << ", " << (condition_ ? "condition" : "null") << ")";
    return oss.str();
}

std::string AggregateNode::to_string() const {
    std::ostringstream oss;
    oss << "Aggregate(" << group_by_.size() << " groups, " << (having_ ? "having" : "no having") << ")";
    return oss.str();
}

std::string SortNode::to_string() const {
    std::ostringstream oss;
    oss << "Sort(" << order_by_.size() << " columns)";
    return oss.str();
}

std::string LimitNode::to_string() const {
    std::ostringstream oss;
    oss << "Limit(" << (limit_ ? "limit" : "no limit") << ", " << (offset_ ? "offset" : "no offset") << ")";
    return oss.str();
}

std::string InsertNode::to_string() const {
    std::ostringstream oss;
    oss << "Insert(" << table_name_ << ", " << columns_.size() << " columns, " << values_.size() << " rows)";
    return oss.str();
}

std::string UpdateNode::to_string() const {
    std::ostringstream oss;
    oss << "Update(" << table_name_ << ", " << set_clause_.size() << " updates)";
    return oss.str();
}

std::string DeleteNode::to_string() const {
    std::ostringstream oss;
    oss << "Delete(" << table_name_ << ", " << (where_condition_ ? "with condition" : "no condition") << ")";
    return oss.str();
}

std::string CreateTableNode::to_string() const {
    std::ostringstream oss;
    oss << "CreateTable(" << table_name_ << ", " << columns_.size() << " columns)";
    return oss.str();
}

std::string DropTableNode::to_string() const {
    std::ostringstream oss;
    oss << "DropTable(" << table_name_ << ")";
    return oss.str();
}

// ExecutionPlan 实现
double ExecutionPlan::get_total_cost() const {
    if (!root_) return 0.0;

    double total_cost = root_->get_cost();
    for (const auto& child : root_->get_children()) {
        ExecutionPlan child_plan(std::move(const_cast<std::unique_ptr<PlanNode>&>(child)));
        total_cost += child_plan.get_total_cost();
    }
    return total_cost;
}

size_t ExecutionPlan::get_total_rows() const {
    if (!root_) return 0;
    return root_->get_estimated_rows();
}

std::string ExecutionPlan::to_string() const {
    if (!root_) return "Empty Plan";

    std::ostringstream oss;
    oss << "ExecutionPlan {\n";
    oss << "  Root: " << root_->to_string() << "\n";
    oss << "  Cost: " << get_total_cost() << "\n";
    oss << "  Estimated Rows: " << get_total_rows() << "\n";
    oss << "}";
    return oss.str();
}

// Planner 实现
Planner::Planner() {
    Logger::info("Planner initialized");
}

std::unique_ptr<ExecutionPlan> Planner::plan(std::unique_ptr<Statement> statement) {
    if (!statement) {
        Logger::error("Cannot plan null statement");
        return nullptr;
    }

    // 根据语句类型生成不同的执行计划
    if (auto select_stmt = dynamic_cast<SelectStatement*>(statement.get())) {
        return plan_select(select_stmt);
    } else if (auto insert_stmt = dynamic_cast<InsertStatement*>(statement.get())) {
        return plan_insert(insert_stmt);
    } else if (auto update_stmt = dynamic_cast<UpdateStatement*>(statement.get())) {
        return plan_update(update_stmt);
    } else if (auto delete_stmt = dynamic_cast<DeleteStatement*>(statement.get())) {
        return plan_delete(delete_stmt);
    } else if (auto create_table_stmt = dynamic_cast<CreateTableStatement*>(statement.get())) {
        return plan_create_table(create_table_stmt);
    } else if (auto drop_table_stmt = dynamic_cast<DropTableStatement*>(statement.get())) {
        return plan_drop_table(drop_table_stmt);
    } else {
        Logger::error("Unknown statement type in planner");
        return nullptr;
    }
}

std::unique_ptr<ExecutionPlan> Planner::plan_select(SelectStatement* stmt) {
    if (!stmt) return nullptr;

    Logger::info("Planning SELECT statement");

    // 获取表名
    auto table_names = stmt->get_from_tables();
    if (table_names.empty()) {
        Logger::error("No tables specified in SELECT statement");
        return nullptr;
    }

    // 创建扫描节点
    std::unique_ptr<PlanNode> current_node = create_scan_node(table_names[0]);

        // 如果有WHERE条件，添加过滤节点
    if (stmt->get_where_clause()) {
        current_node = create_filter_node(std::move(current_node),
                                        std::unique_ptr<Expression>(stmt->get_where_clause()->clone()));
    }

    // 如果有GROUP BY，添加聚合节点
    if (!stmt->get_group_by().empty()) {
        std::vector<std::unique_ptr<Expression>> group_by;
        for (const auto& expr : stmt->get_group_by()) {
            group_by.push_back(expr->clone());
        }
        current_node = create_aggregate_node(std::move(current_node),
                                           std::move(group_by),
                                           stmt->get_having_clause() ? std::unique_ptr<Expression>(stmt->get_having_clause()->clone()) : nullptr);
    }

    // 如果有ORDER BY，添加排序节点
    if (!stmt->get_order_by().empty()) {
        std::vector<std::unique_ptr<Expression>> order_by;
        for (const auto& expr : stmt->get_order_by()) {
            order_by.push_back(expr->clone());
        }
        current_node = create_sort_node(std::move(current_node),
                                      std::move(order_by));
    }

    // 如果有LIMIT，添加限制节点
    if (stmt->get_limit()) {
        current_node = create_limit_node(std::move(current_node),
                                       std::unique_ptr<Expression>(stmt->get_limit()->clone()),
                                       stmt->get_offset() ? std::unique_ptr<Expression>(stmt->get_offset()->clone()) : nullptr);
    }

    // 添加投影节点
    std::vector<std::unique_ptr<Expression>> select_list;
    for (const auto& expr : stmt->get_select_list()) {
        select_list.push_back(expr->clone());
    }
    current_node = create_project_node(std::move(current_node),
                                     std::move(select_list));

    return std::make_unique<ExecutionPlan>(std::move(current_node));
}

std::unique_ptr<ExecutionPlan> Planner::plan_insert(InsertStatement* stmt) {
    if (!stmt) return nullptr;

    Logger::info("Planning INSERT statement");

    auto node = std::make_unique<InsertNode>(
        stmt->get_table_name(),
        std::vector<std::string>(stmt->get_columns()),
        std::vector<std::vector<std::unique_ptr<Expression>>>() // 简化实现
    );

    return std::make_unique<ExecutionPlan>(std::move(node));
}

std::unique_ptr<ExecutionPlan> Planner::plan_update(UpdateStatement* stmt) {
    if (!stmt) return nullptr;

    Logger::info("Planning UPDATE statement");

    auto node = std::make_unique<UpdateNode>(
        stmt->get_table_name(),
        std::vector<std::pair<std::string, std::unique_ptr<Expression>>>(), // 简化实现
        stmt->get_where_clause() ? std::unique_ptr<Expression>(stmt->get_where_clause()->clone()) : nullptr
    );

    return std::make_unique<ExecutionPlan>(std::move(node));
}

std::unique_ptr<ExecutionPlan> Planner::plan_delete(DeleteStatement* stmt) {
    if (!stmt) return nullptr;

    Logger::info("Planning DELETE statement");

    auto node = std::make_unique<DeleteNode>(
        stmt->get_table_name(),
        stmt->get_where_clause() ? std::unique_ptr<Expression>(stmt->get_where_clause()->clone()) : nullptr
    );

    return std::make_unique<ExecutionPlan>(std::move(node));
}

std::unique_ptr<ExecutionPlan> Planner::plan_create_table(CreateTableStatement* stmt) {
    if (!stmt) return nullptr;

    Logger::info("Planning CREATE TABLE statement");

    auto node = std::make_unique<CreateTableNode>(
        stmt->get_table_name(),
        std::vector<CreateTableStatement::ColumnDefinition>() // 简化实现
    );

    return std::make_unique<ExecutionPlan>(std::move(node));
}

std::unique_ptr<ExecutionPlan> Planner::plan_drop_table(DropTableStatement* stmt) {
    if (!stmt) return nullptr;

    Logger::info("Planning DROP TABLE statement");

    auto node = std::make_unique<DropTableNode>(stmt->get_table_name());

    return std::make_unique<ExecutionPlan>(std::move(node));
}

// 辅助方法实现
std::unique_ptr<PlanNode> Planner::create_scan_node(const std::string& table_name) {
    auto node = std::make_unique<ScanNode>(table_name);
    node->set_cost(100.0); // 基础扫描成本
    node->set_estimated_rows(1000); // 估算行数
    return node;
}

std::unique_ptr<PlanNode> Planner::create_filter_node(std::unique_ptr<PlanNode> child,
                                                     std::unique_ptr<Expression> condition) {
    auto node = std::make_unique<FilterNode>(std::move(condition));
    node->add_child(std::move(child));
    node->set_cost(50.0); // 过滤成本
    node->set_estimated_rows(500); // 估算过滤后的行数
    return node;
}

std::unique_ptr<PlanNode> Planner::create_project_node(std::unique_ptr<PlanNode> child,
                                                      std::vector<std::unique_ptr<Expression>> expressions) {
    auto node = std::make_unique<ProjectNode>(std::move(expressions));
    node->add_child(std::move(child));
    node->set_cost(10.0); // 投影成本
    node->set_estimated_rows(500); // 保持行数不变
    return node;
}

std::unique_ptr<PlanNode> Planner::create_join_node(std::unique_ptr<PlanNode> left,
                                                   std::unique_ptr<PlanNode> right,
                                                   std::unique_ptr<Expression> condition,
                                                   JoinType join_type) {
    auto node = std::make_unique<JoinNode>(join_type, std::move(condition));
    node->add_child(std::move(left));
    node->add_child(std::move(right));
    node->set_cost(200.0); // 连接成本
    node->set_estimated_rows(1000); // 估算连接后的行数
    return node;
}

std::unique_ptr<PlanNode> Planner::create_aggregate_node(std::unique_ptr<PlanNode> child,
                                                        std::vector<std::unique_ptr<Expression>> group_by,
                                                        std::unique_ptr<Expression> having) {
    auto node = std::make_unique<AggregateNode>(std::move(group_by), std::move(having));
    node->add_child(std::move(child));
    node->set_cost(150.0); // 聚合成本
    node->set_estimated_rows(100); // 估算聚合后的行数
    return node;
}

std::unique_ptr<PlanNode> Planner::create_sort_node(std::unique_ptr<PlanNode> child,
                                                   std::vector<std::unique_ptr<Expression>> order_by) {
    auto node = std::make_unique<SortNode>(std::move(order_by));
    node->add_child(std::move(child));
    node->set_cost(300.0); // 排序成本
    node->set_estimated_rows(500); // 保持行数不变
    return node;
}

std::unique_ptr<PlanNode> Planner::create_limit_node(std::unique_ptr<PlanNode> child,
                                                    std::unique_ptr<Expression> limit,
                                                    std::unique_ptr<Expression> offset) {
    auto node = std::make_unique<LimitNode>(std::move(limit), std::move(offset));
    node->add_child(std::move(child));
    node->set_cost(5.0); // 限制成本
    node->set_estimated_rows(10); // 估算限制后的行数
    return node;
}

} // namespace sealdb
