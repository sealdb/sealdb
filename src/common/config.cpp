#include "sealdb/config.h"
#include <fstream>
#include <iostream>
#include <cstdlib>
#include <algorithm>

namespace sealdb {

class Config::Impl {
public:
    std::map<std::string, std::string> config_map_;
};

Config::Config() : pimpl_(std::make_unique<Impl>()) {
}

Config::Config(const Config& other) : pimpl_(std::make_unique<Impl>(*other.pimpl_)) {
}

Config::Config(Config&& other) noexcept : pimpl_(std::move(other.pimpl_)) {
}

Config::~Config() = default;

Config& Config::operator=(const Config& other) {
    if (this != &other) {
        *pimpl_ = *other.pimpl_;
    }
    return *this;
}

Config& Config::operator=(Config&& other) noexcept {
    if (this != &other) {
        pimpl_ = std::move(other.pimpl_);
    }
    return *this;
}

bool Config::load_from_file(const std::string& filename) {
    std::ifstream file(filename);
    if (!file.is_open()) {
        return false;
    }

    std::string line;
    while (std::getline(file, line)) {
        // 跳过注释和空行
        if (line.empty() || line[0] == '#') {
            continue;
        }

        size_t pos = line.find('=');
        if (pos != std::string::npos) {
            std::string key = line.substr(0, pos);
            std::string value = line.substr(pos + 1);

            // 去除首尾空格
            key.erase(0, key.find_first_not_of(" \t"));
            key.erase(key.find_last_not_of(" \t") + 1);
            value.erase(0, value.find_first_not_of(" \t"));
            value.erase(value.find_last_not_of(" \t") + 1);

            pimpl_->config_map_[key] = value;
        }
    }

    return true;
}

bool Config::load_from_env() {
    // 这里可以添加从环境变量加载配置的逻辑
    return true;
}

std::string Config::get_string(const std::string& key, const std::string& default_value) const {
    auto it = pimpl_->config_map_.find(key);
    return (it != pimpl_->config_map_.end()) ? it->second : default_value;
}

int Config::get_int(const std::string& key, int default_value) const {
    auto it = pimpl_->config_map_.find(key);
    if (it != pimpl_->config_map_.end()) {
        try {
            return std::stoi(it->second);
        } catch (...) {
            return default_value;
        }
    }
    return default_value;
}

bool Config::get_bool(const std::string& key, bool default_value) const {
    auto it = pimpl_->config_map_.find(key);
    if (it != pimpl_->config_map_.end()) {
        std::string value = it->second;
        std::transform(value.begin(), value.end(), value.begin(), ::tolower);
        return (value == "true" || value == "1" || value == "yes");
    }
    return default_value;
}

void Config::set(const std::string& key, const std::string& value) {
    pimpl_->config_map_[key] = value;
}

bool Config::has(const std::string& key) const {
    return pimpl_->config_map_.find(key) != pimpl_->config_map_.end();
}

} // namespace sealdb