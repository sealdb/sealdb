#ifndef SEALDB_TIKV_CLIENT_H
#define SEALDB_TIKV_CLIENT_H

#include <string>
#include <memory>

namespace sealdb {

class TiKVClient {
public:
    TiKVClient();
    ~TiKVClient();

    bool connect(const std::string& pd_endpoints);
    void disconnect();

private:
    class Impl;
    std::unique_ptr<Impl> pimpl_;
};

} // namespace sealdb

#endif // SEALDB_TIKV_CLIENT_H
