ROOT:=$(shell pwd)
NAME:=github.com/forcedb/forcedb
#export GOPATH := $(shell pwd):$(GOPATH)
GIT_TAG=$(shell git describe --tags --always || echo "git not found")
BUILD_TIME=$(shell date "+%Y-%m-%d_%H:%M:%S")
LDFLAGS="-X github.com/forcedb/forcedb/version.gitTag=${GIT_TAG} -X github.com/forcedb/forcedb/version.buildTime=${BUILD_TIME}"

initmod:
	rm -f go.mod go.sum
	go mod init ${NAME}
	go mod tidy
	go mod vendor

build: ast
	@mkdir -p bin/
	go build -v -o bin/forcedb --ldflags $(LDFLAGS) server/main.go
	@chmod 755 bin/*

fmt:
	go fmt ./...
	go vet ./...

ast:
	cd ${ROOT}/sqlparser && $(MAKE) sql.go

test:
	$(MAKE) testsqlparser

testsqlparser: ast
	go test -v ${ROOT}/sqlparser

# code coverage
allpkgs = \
	${ROOT}/sqlparser/...\
	#${ROOT}/server/...
covout = /tmp/coverage.out

coverage:
	go build -v -o bin/gotestcover vendor/github.com/pierrre/gotestcover/*.go
	bin/gotestcover -coverprofile=$(covout) -v $(allpkgs)
	go tool cover -html=$(covout)

clean:
	cd ${ROOT}/sqlparser && $(MAKE) clean

check:
	go get -v gopkg.in/alecthomas/gometalinter.v2
	bin/gometalinter.v2 -j 4 --disable-all \
	--enable=gofmt \
	--enable=golint \
	--enable=vet \
	--enable=gosimple \
	--enable=unconvert \
	--deadline=10m $(allpkgs) 2>&1 | tee /dev/stderr

.PHONY: initmod build clean install fmt ast test coverage check
