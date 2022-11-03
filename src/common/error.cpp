#include "error.h"
#include <sstream>

namespace sealdb {

Error::Error(ErrorCode code, const std::string& message)
    : code_(code), message_(message) {
}

std::string Error::to_string() const {
    std::ostringstream oss;
    oss << "Error " << static_cast<int>(code_) << ": " << message_;
    return oss.str();
}

} // namespace sealdb