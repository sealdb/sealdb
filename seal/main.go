/*
 * Copyright 2022-2025 The Seal Authors.

 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at

       http://www.apache.org/licenses/LICENSE-2.0

 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
*/

package main

import (
	"fmt"

	"github.com/sealdb/seal/sqlparser"
	"github.com/sealdb/seal/version"
)

func main() {
	fmt.Println(*version.GetBanner())
	fmt.Printf("version: [%+v]\n", version.GetVersion())

	testcases := []struct {
		input  string
		output string
		err    error
	}{
		{
			input:  "select 1",
			output: "select 1 from dual",
		}, {
			input: "",
			err:   sqlparser.ErrEmpty,
		}, {
			input: ";",
			err:   sqlparser.ErrEmpty,
		}, {
			input:  "-- sdf",
			output: "-- sdf",
		}, {
			input:  "/* sdf */",
			output: "/* sdf */",
		}, {
			input:  "# sdf",
			output: "# sdf",
		}, {
			input:  "/* sdf */ select 1",
			output: "select 1 from dual",
		},
	}
	for _, testcase := range testcases {
		res, err := sqlparser.Parse(testcase.input)
		fmt.Printf("testcase.err: %v, err: %v\n", testcase.err, err)
		fmt.Printf("output: %v, res_str: %s\n", testcase.output, sqlparser.String(res))

		res, err = sqlparser.ParseStrictDDL(testcase.input)
		fmt.Printf("testcase.err: %v, err: %v\n", testcase.err, err)
		fmt.Printf("output: %v, res_str: %s\n", testcase.output, sqlparser.String(res))
	}
}
