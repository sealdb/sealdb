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

package version

import (
	"fmt"
	"runtime"
)

var (
	projectName = "sealdb"
	major       = 1
	minor       = 0
	patch       = 0

	// the backend MySQL version
	mysqlMajor = 8
	mysqlMinor = 0
	mysqlPatch = 29

	gitTag    = "Not provided"
	buildTime = "Not privided"
)

type Version struct {
	ProjectName string
	Major       int
	Minor       int
	Patch       int
	MysqlMajor  int
	MysqlMinor  int
	MysqlPatch  int
	GitTag      string
	BuildTime   string
	GoVersion   string
	Platform    string
}

// GetVersion returns the version.
func GetVersion() *Version {
	return &Version{
		ProjectName: projectName,
		Major:       major,
		Minor:       minor,
		Patch:       patch,
		MysqlMajor:  mysqlMajor,
		MysqlMinor:  mysqlMinor,
		MysqlPatch:  mysqlPatch,
		GitTag:      gitTag,
		BuildTime:   buildTime,
		GoVersion:   runtime.Version(),
		Platform:    fmt.Sprintf("%s/%s", runtime.GOOS, runtime.GOARCH),
	}
}

//func GetVersion() string {
//	return fmt.Sprintf("%d.%d.%d", Major, Minor, Patch)
//}
//
//func GetVersionFull() string {
//	return fmt.Sprintf("%d.%d.%d-%s-%d.%d.%d %s %s", MySQLMajor, MySQLMinor,
//		MySQLPatch, ProjectName, Major, Minor, Patch, GoVersion, Platform)
//}
