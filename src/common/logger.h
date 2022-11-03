#ifndef SEALDB_LOGGER_H
#define SEALDB_LOGGER_H

#include <string>

namespace sealdb {

enum class LogLevel {
    DEBUG,
    INFO,
    WARN,
    ERROR
};

class Logger {
public:
    static void log(LogLevel level, const std::string& message);
    static void debug(const std::string& message);
    static void info(const std::string& message);
    static void warn(const std::string& message);
    static void error(const std::string& message);
};

} // namespace sealdb

#endif // SEALDB_LOGGER_H