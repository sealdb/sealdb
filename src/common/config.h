#ifndef SEALDB_CONFIG_H
#define SEALDB_CONFIG_H

#include <string>
#include <map>
#include <memory>

namespace sealdb {

/**
 * @brief 配置管理类
 */
class Config {
public:
    Config();
    ~Config();

    /**
     * @brief 从文件加载配置
     * @param filename 配置文件路径
     * @return 是否成功
     */
    bool load_from_file(const std::string& filename);

    /**
     * @brief 从环境变量加载配置
     * @return 是否成功
     */
    bool load_from_env();

    /**
     * @brief 获取字符串配置
     * @param key 配置键
     * @param default_value 默认值
     * @return 配置值
     */
    std::string get_string(const std::string& key, const std::string& default_value = "") const;

    /**
     * @brief 获取整数配置
     * @param key 配置键
     * @param default_value 默认值
     * @return 配置值
     */
    int get_int(const std::string& key, int default_value = 0) const;

    /**
     * @brief 获取布尔配置
     * @param key 配置键
     * @param default_value 默认值
     * @return 配置值
     */
    bool get_bool(const std::string& key, bool default_value = false) const;

    /**
     * @brief 设置配置
     * @param key 配置键
     * @param value 配置值
     */
    void set(const std::string& key, const std::string& value);

    /**
     * @brief 检查配置是否存在
     * @param key 配置键
     * @return 是否存在
     */
    bool has(const std::string& key) const;

private:
    class Impl;
    std::unique_ptr<Impl> pimpl_;
};

} // namespace sealdb

#endif // SEALDB_CONFIG_H