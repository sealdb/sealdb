#include "statistics_manager.h"
#include <algorithm>
#include <cmath>

namespace sealdb {

StatisticsManager::StatisticsManager() {
    // 初始化统计信息管理器
}

const TableStats* StatisticsManager::get_table_stats(const std::string& table_name) const {
    auto it = table_stats_.find(table_name);
    return it != table_stats_.end() ? &it->second : nullptr;
}

const ColumnStats* StatisticsManager::get_column_stats(const std::string& table_name,
                                                     const std::string& column_name) const {
    auto table_it = table_stats_.find(table_name);
    if (table_it == table_stats_.end()) {
        return nullptr;
    }

    auto column_it = table_it->second.column_stats.find(column_name);
    return column_it != table_it->second.column_stats.end() ? &column_it->second : nullptr;
}

const IndexStats* StatisticsManager::get_index_stats(const std::string& index_name) const {
    auto it = index_stats_.find(index_name);
    return it != index_stats_.end() ? &it->second : nullptr;
}

void StatisticsManager::update_table_stats(const std::string& table_name, const TableStats& stats) {
    table_stats_[table_name] = stats;
}

void StatisticsManager::update_column_stats(const std::string& table_name,
                                          const std::string& column_name,
                                          const ColumnStats& stats) {
    table_stats_[table_name].column_stats[column_name] = stats;
}

void StatisticsManager::update_index_stats(const std::string& index_name, const IndexStats& stats) {
    index_stats_[index_name] = stats;
}

void StatisticsManager::analyze_table(const std::string& table_name) {
    // TODO: 实现表统计信息分析
    // 这里应该扫描表数据，计算各种统计信息
}

void StatisticsManager::analyze_index(const std::string& index_name) {
    // TODO: 实现索引统计信息分析
    // 这里应该扫描索引数据，计算各种统计信息
}

double StatisticsManager::estimate_selectivity(const std::string& table_name,
                                            const std::string& column_name,
                                            const std::string& op,
                                            const std::string& value) const {
    const ColumnStats* stats = get_column_stats(table_name, column_name);
    if (!stats) {
        // 如果没有统计信息，返回默认值
        return 0.1; // 10% 的默认选择率
    }

    return calculate_column_selectivity(*stats, op, value);
}

size_t StatisticsManager::estimate_cardinality(const std::string& table_name,
                                             const std::string& column_name,
                                             const std::string& op,
                                             const std::string& value) const {
    const TableStats* table_stats = get_table_stats(table_name);
    if (!table_stats) {
        return 0;
    }

    double selectivity = estimate_selectivity(table_name, column_name, op, value);
    return static_cast<size_t>(table_stats->row_count * selectivity);
}

size_t StatisticsManager::estimate_join_cardinality(const std::string& left_table,
                                                  const std::string& left_column,
                                                  const std::string& right_table,
                                                  const std::string& right_column) const {
    const TableStats* left_stats = get_table_stats(left_table);
    const TableStats* right_stats = get_table_stats(right_table);

    if (!left_stats || !right_stats) {
        return 0;
    }

    // 简单的连接基数估算
    // 假设连接条件的选择率为较小表的1/distinct_values
    const ColumnStats* left_col_stats = get_column_stats(left_table, left_column);
    const ColumnStats* right_col_stats = get_column_stats(right_table, right_column);

    if (!left_col_stats || !right_col_stats) {
        // 如果没有列统计信息，使用简单的估算
        return std::min(left_stats->row_count, right_stats->row_count);
    }

    double left_selectivity = 1.0 / left_col_stats->distinct_values;
    double right_selectivity = 1.0 / right_col_stats->distinct_values;
    double join_selectivity = std::min(left_selectivity, right_selectivity);

    return static_cast<size_t>(left_stats->row_count * right_stats->row_count * join_selectivity);
}

double StatisticsManager::calculate_column_selectivity(const ColumnStats& stats,
                                                   const std::string& op,
                                                   const std::string& value) const {
    // 简单的选择性计算
    // 实际实现中应该考虑直方图、最常见值等统计信息

    if (op == "=") {
        // 等值查询的选择性
        return 1.0 / stats.distinct_values;
    } else if (op == ">" || op == ">=") {
        // 范围查询的选择性
        return 0.3; // 简化的估算
    } else if (op == "<" || op == "<=") {
        // 范围查询的选择性
        return 0.3; // 简化的估算
    } else if (op == "!=") {
        // 不等值查询的选择性
        return 1.0 - (1.0 / stats.distinct_values);
    } else if (op == "LIKE") {
        // LIKE查询的选择性
        return 0.1; // 简化的估算
    }

    return 0.1; // 默认选择率
}

double StatisticsManager::calculate_index_selectivity(const IndexStats& stats,
                                                   const std::vector<std::string>& conditions) const {
    // 索引选择性计算
    // 实际实现中应该考虑索引的统计信息

    if (conditions.empty()) {
        return 1.0; // 无条件时返回全表扫描
    }

    // 简化的索引选择性计算
    double selectivity = 1.0;
    for (size_t i = 0; i < std::min(conditions.size(), stats.columns.size()); ++i) {
        selectivity *= 1.0 / stats.distinct_values;
    }

    return selectivity;
}

} // namespace sealdb