#include "buffer.h"
#include <cstring>
#include <algorithm>

namespace sealdb {

Buffer::Buffer() : read_pos_(0) {
}

Buffer::Buffer(size_t initial_size) : data_(initial_size), read_pos_(0) {
}

void Buffer::write(const void* data, size_t size) {
    if (size == 0) return;

    size_t old_size = data_.size();
    data_.resize(old_size + size);
    std::memcpy(data_.data() + old_size, data, size);
}

size_t Buffer::read(void* data, size_t size) {
    size_t readable = readable_size();
    size_t read_size = std::min(size, readable);

    if (read_size > 0) {
        std::memcpy(data, readable_data(), read_size);
        read_pos_ += read_size;
    }

    return read_size;
}

void Buffer::clear() {
    data_.clear();
    read_pos_ = 0;
}

} // namespace sealdb
