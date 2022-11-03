#ifndef SEALDB_UTILS_H
#define SEALDB_UTILS_H

#include <string>
#include <vector>

namespace sealdb {

namespace utils {

/**
 * @brief 字符串分割
 * @param str 要分割的字符串
 * @param delimiter 分隔符
 * @return 分割后的字符串数组
 */
std::vector<std::string> split(const std::string& str, const std::string& delimiter);

/**
 * @brief 字符串去除首尾空格
 * @param str 要处理的字符串
 * @return 处理后的字符串
 */
std::string trim(const std::string& str);

/**
 * @brief 字符串转小写
 * @param str 要转换的字符串
 * @return 转换后的字符串
 */
std::string to_lower(const std::string& str);

/**
 * @brief 字符串转大写
 * @param str 要转换的字符串
 * @return 转换后的字符串
 */
std::string to_upper(const std::string& str);

} // namespace utils

} // namespace sealdb

#endif // SEALDB_UTILS_H
