#ifndef SEALDB_PHYSICAL_OPERATOR_H
#define SEALDB_PHYSICAL_OPERATOR_H

#include "sealdb/planner.h"
#include <memory>
#include <vector>
#include <string>

namespace sealdb {

/**
 * 物理操作符基类
 */
class PhysicalOperator {
public:
    PhysicalOperator() = default;
    virtual ~PhysicalOperator() = default;

    /**
     * 初始化操作符
     */
    virtual bool init() = 0;

    /**
     * 获取下一行数据
     */
    virtual bool next() = 0;

    /**
     * 获取当前行数据
     */
    virtual const std::vector<std::string>& get_current_row() const = 0;

    /**
     * 关闭操作符
     */
    virtual void close() = 0;

    /**
     * 获取操作符类型
     */
    virtual std::string get_type() const = 0;
};

/**
 * 表扫描操作符
 */
class TableScanOperator : public PhysicalOperator {
public:
    explicit TableScanOperator(const std::string& table_name);
    ~TableScanOperator() override = default;

    bool init() override;
    bool next() override;
    const std::vector<std::string>& get_current_row() const override;
    void close() override;
    std::string get_type() const override { return "TableScan"; }

private:
    std::string table_name_;
    std::vector<std::string> current_row_;
    bool initialized_;
    // TODO: 添加表扫描的具体实现
};

/**
 * 索引扫描操作符
 */
class IndexScanOperator : public PhysicalOperator {
public:
    IndexScanOperator(const std::string& table_name, const std::string& index_name);
    ~IndexScanOperator() override = default;

    bool init() override;
    bool next() override;
    const std::vector<std::string>& get_current_row() const override;
    void close() override;
    std::string get_type() const override { return "IndexScan"; }

private:
    std::string table_name_;
    std::string index_name_;
    std::vector<std::string> current_row_;
    bool initialized_;
    // TODO: 添加索引扫描的具体实现
};

/**
 * 过滤操作符
 */
class FilterOperator : public PhysicalOperator {
public:
    FilterOperator(std::unique_ptr<PhysicalOperator> child,
                  std::unique_ptr<Expression> condition);
    ~FilterOperator() override = default;

    bool init() override;
    bool next() override;
    const std::vector<std::string>& get_current_row() const override;
    void close() override;
    std::string get_type() const override { return "Filter"; }

private:
    std::unique_ptr<PhysicalOperator> child_;
    std::unique_ptr<Expression> condition_;
    std::vector<std::string> current_row_;
    bool initialized_;
    // TODO: 添加过滤的具体实现
};

/**
 * 投影操作符
 */
class ProjectOperator : public PhysicalOperator {
public:
    ProjectOperator(std::unique_ptr<PhysicalOperator> child,
                   std::vector<std::unique_ptr<Expression>> expressions);
    ~ProjectOperator() override = default;

    bool init() override;
    bool next() override;
    const std::vector<std::string>& get_current_row() const override;
    void close() override;
    std::string get_type() const override { return "Project"; }

private:
    std::unique_ptr<PhysicalOperator> child_;
    std::vector<std::unique_ptr<Expression>> expressions_;
    std::vector<std::string> current_row_;
    bool initialized_;
    // TODO: 添加投影的具体实现
};

/**
 * 连接操作符
 */
class JoinOperator : public PhysicalOperator {
public:
    JoinOperator(std::unique_ptr<PhysicalOperator> left,
                std::unique_ptr<PhysicalOperator> right,
                std::unique_ptr<Expression> condition,
                JoinType join_type);
    ~JoinOperator() override = default;

    bool init() override;
    bool next() override;
    const std::vector<std::string>& get_current_row() const override;
    void close() override;
    std::string get_type() const override { return "Join"; }

private:
    std::unique_ptr<PhysicalOperator> left_;
    std::unique_ptr<PhysicalOperator> right_;
    std::unique_ptr<Expression> condition_;
    JoinType join_type_;
    std::vector<std::string> current_row_;
    bool initialized_;
    // TODO: 添加连接的具体实现
};

/**
 * 聚合操作符
 */
class AggregateOperator : public PhysicalOperator {
public:
    AggregateOperator(std::unique_ptr<PhysicalOperator> child,
                     std::vector<std::unique_ptr<Expression>> group_by,
                     std::unique_ptr<Expression> having);
    ~AggregateOperator() override = default;

    bool init() override;
    bool next() override;
    const std::vector<std::string>& get_current_row() const override;
    void close() override;
    std::string get_type() const override { return "Aggregate"; }

private:
    std::unique_ptr<PhysicalOperator> child_;
    std::vector<std::unique_ptr<Expression>> group_by_;
    std::unique_ptr<Expression> having_;
    std::vector<std::string> current_row_;
    bool initialized_;
    // TODO: 添加聚合的具体实现
};

/**
 * 排序操作符
 */
class SortOperator : public PhysicalOperator {
public:
    SortOperator(std::unique_ptr<PhysicalOperator> child,
                std::vector<std::unique_ptr<Expression>> order_by);
    ~SortOperator() override = default;

    bool init() override;
    bool next() override;
    const std::vector<std::string>& get_current_row() const override;
    void close() override;
    std::string get_type() const override { return "Sort"; }

private:
    std::unique_ptr<PhysicalOperator> child_;
    std::vector<std::unique_ptr<Expression>> order_by_;
    std::vector<std::string> current_row_;
    bool initialized_;
    // TODO: 添加排序的具体实现
};

/**
 * 限制操作符
 */
class LimitOperator : public PhysicalOperator {
public:
    LimitOperator(std::unique_ptr<PhysicalOperator> child,
                 std::unique_ptr<Expression> limit,
                 std::unique_ptr<Expression> offset);
    ~LimitOperator() override = default;

    bool init() override;
    bool next() override;
    const std::vector<std::string>& get_current_row() const override;
    void close() override;
    std::string get_type() const override { return "Limit"; }

private:
    std::unique_ptr<PhysicalOperator> child_;
    std::unique_ptr<Expression> limit_;
    std::unique_ptr<Expression> offset_;
    std::vector<std::string> current_row_;
    bool initialized_;
    size_t rows_returned_;
    // TODO: 添加限制的具体实现
};

} // namespace sealdb

#endif // SEALDB_PHYSICAL_OPERATOR_H