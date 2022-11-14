ROOT:=$(shell pwd)
NAME:=github.com/forcedb/forcedb
#export GOPATH := $(shell pwd):$(GOPATH)

initmod:
	rm -f go.mod go.sum
	go mod init ${NAME}
	go mod tidy
	go mod vendor

fmt:
	go fmt ./...
	go vet ./...

ast:
	cd ${ROOT}/sqlparser && $(MAKE) sql.go

test: 
	@echo "--> Testing..."
	$(MAKE) testsqlparser

testsqlparser: ast
	go test -v ${ROOT}/sqlparser

# code coverage
allpkgs = \
	${ROOT}/base/...\
	${ROOT}/sqlparser/...
covout = /tmp/coverage.out

coverage:
	go build -v -o bin/gotestcover vendor/github.com/pierrre/gotestcover/*.go
	bin/gotestcover -coverprofile=$(covout) -v $(allpkgs)
	go tool cover -html=$(covout)

clean:
	cd ${ROOT}/sqlparser && $(MAKE) clean
