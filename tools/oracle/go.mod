module sealdb/tools/oracle

go 1.25.12

require github.com/pingcap/tidb/pkg/parser v0.0.0-20211011031125-9b13dc409c5e

require (
	github.com/davecgh/go-spew v1.1.2-0.20180830191138-d8f796af33cc // indirect
	github.com/pingcap/errors v0.11.5-0.20250523034308-74f78ae071ee // indirect
	github.com/pingcap/failpoint v0.0.0-20240528011301-b51a646c7c86 // indirect
	github.com/pingcap/log v1.1.1-0.20250917021125-19901e015dc9 // indirect
	github.com/pmezard/go-difflib v1.0.1-0.20181226105442-5d4384ee4fb2 // indirect
	github.com/stretchr/testify v1.11.1 // indirect
	go.uber.org/atomic v1.11.0 // indirect
	go.uber.org/multierr v1.11.0 // indirect
	go.uber.org/zap v1.27.1 // indirect
	golang.org/x/text v0.39.0 // indirect
	gopkg.in/natefinch/lumberjack.v2 v2.2.1 // indirect
)

replace (
	github.com/pingcap/tidb => ../../../../pingcap/tidb
	github.com/pingcap/tidb/pkg/parser => ../../../../pingcap/tidb/pkg/parser
)
