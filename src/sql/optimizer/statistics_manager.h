#ifndef SEALDB_STATISTICS_MANAGER_H
#define SEALDB_STATISTICS_MANAGER_H

#include <string>
#include <unordered_map>
#include <vector>
#include <memory>

namespace sealdb {

/**
 * 列统计信息
 */
struct ColumnStats {
    double distinct_values;      // 不同值数量
    double min_value;           // 最小值
    double max_value;           // 最大值
    double null_fraction;       // NULL值比例
    double avg_width;           // 平均宽度
    std::vector<double> most_common_values;  // 最常见的值
    std::vector<double> most_common_freqs;   // 对应频率
    std::vector<double> histogram_bounds;    // 直方图边界
};

/**
 * 表统计信息
 */
struct TableStats {
    size_t row_count;           // 行数
    size_t page_count;          // 页数
    double avg_row_size;        // 平均行大小
    std::unordered_map<std::string, ColumnStats> column_stats;  // 列统计信息
    double last_analyzed;       // 最后分析时间
};

/**
 * 索引统计信息
 */
struct IndexStats {
    std::string table_name;
    std::string index_name;
    std::vector<std::string> columns;
    size_t height;              // B+树高度
    size_t leaf_pages;          // 叶子页数
    double selectivity;         // 选择性
    double distinct_values;     // 不同值数量
};

/**
 * 统计信息管理器
 *
 * 负责管理和维护数据库的统计信息，为CBO提供数据支持
 */
class StatisticsManager {
public:
    StatisticsManager();
    ~StatisticsManager() = default;

    /**
     * 获取表统计信息
     */
    const TableStats* get_table_stats(const std::string& table_name) const;

    /**
     * 获取列统计信息
     */
    const ColumnStats* get_column_stats(const std::string& table_name,
                                       const std::string& column_name) const;

    /**
     * 获取索引统计信息
     */
    const IndexStats* get_index_stats(const std::string& index_name) const;

    /**
     * 更新表统计信息
     */
    void update_table_stats(const std::string& table_name, const TableStats& stats);

    /**
     * 更新列统计信息
     */
    void update_column_stats(const std::string& table_name,
                           const std::string& column_name,
                           const ColumnStats& stats);

    /**
     * 更新索引统计信息
     */
    void update_index_stats(const std::string& index_name, const IndexStats& stats);

    /**
     * 分析表统计信息
     */
    void analyze_table(const std::string& table_name);

    /**
     * 分析索引统计信息
     */
    void analyze_index(const std::string& index_name);

    /**
     * 估算选择率
     */
    double estimate_selectivity(const std::string& table_name,
                              const std::string& column_name,
                              const std::string& op,
                              const std::string& value) const;

    /**
     * 估算基数
     */
    size_t estimate_cardinality(const std::string& table_name,
                               const std::string& column_name,
                               const std::string& op,
                               const std::string& value) const;

    /**
     * 估算连接基数
     */
    size_t estimate_join_cardinality(const std::string& left_table,
                                   const std::string& left_column,
                                   const std::string& right_table,
                                   const std::string& right_column) const;

private:
    std::unordered_map<std::string, TableStats> table_stats_;
    std::unordered_map<std::string, IndexStats> index_stats_;

    /**
     * 计算列的选择性
     */
    double calculate_column_selectivity(const ColumnStats& stats,
                                     const std::string& op,
                                     const std::string& value) const;

    /**
     * 计算索引的选择性
     */
    double calculate_index_selectivity(const IndexStats& stats,
                                    const std::vector<std::string>& conditions) const;
};

} // namespace sealdb

#endif // SEALDB_STATISTICS_MANAGER_H