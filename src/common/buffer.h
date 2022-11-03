#ifndef SEALDB_BUFFER_H
#define SEALDB_BUFFER_H

#include <vector>
#include <cstdint>

namespace sealdb {

/**
 * @brief 缓冲区类
 */
class Buffer {
public:
    Buffer();
    Buffer(size_t initial_size);
    ~Buffer() = default;

    /**
     * @brief 写入数据
     * @param data 数据指针
     * @param size 数据大小
     */
    void write(const void* data, size_t size);

    /**
     * @brief 读取数据
     * @param data 数据指针
     * @param size 数据大小
     * @return 实际读取的字节数
     */
    size_t read(void* data, size_t size);

    /**
     * @brief 获取缓冲区大小
     * @return 缓冲区大小
     */
    size_t size() const { return data_.size(); }

    /**
     * @brief 获取可读数据大小
     * @return 可读数据大小
     */
    size_t readable_size() const { return size() - read_pos_; }

    /**
     * @brief 清空缓冲区
     */
    void clear();

    /**
     * @brief 获取数据指针
     * @return 数据指针
     */
    const uint8_t* data() const { return data_.data(); }

    /**
     * @brief 获取可读数据指针
     * @return 可读数据指针
     */
    const uint8_t* readable_data() const { return data_.data() + read_pos_; }

private:
    std::vector<uint8_t> data_;
    size_t read_pos_;
};

} // namespace sealdb

#endif // SEALDB_BUFFER_H
