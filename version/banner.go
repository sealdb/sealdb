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
	"math/rand"
	"time"
)

var banners = [3]string{
	// modular
	`
 _______  _______  _______  ___      ______   _______
|       ||       ||   _   ||   |    |      | |  _    |
|  _____||    ___||  |_|  ||   |    |  _    || |_|   |
| |_____ |   |___ |       ||   |    | | |   ||       |
|_____  ||    ___||       ||   |___ | |_|   ||  _   |
 _____| ||   |___ |   _   ||       ||       || |_|   |
|_______||_______||__| |__||_______||______| |_______|
`,
	`
 _______  _______  _______  _        ______   ______
(  ____ \(  ____ \(  ___  )( \      (  __  \ (  ___ \
| (    \/| (    \/| (   ) || (      | (  \  )| (   ) )
| (_____ | (__    | (___) || |      | |   ) || (__/ /
(_____  )|  __)   |  ___  || |      | |   | ||  __ (
      ) || (      | (   ) || |      | |   ) || (  \ \
/\____) || (____/\| )   ( || (____/\| (__/  )| )___) )
\_______)(_______/|/     \|(_______/(______/ |/ \___/
`,
	// starwars
	`
     _______. _______     ___       __       _______  .______
    /       ||   ____|   /   \     |  |     |       \ |   _  \
   |   (----||  |__     /  ^  \    |  |     |  .--.  ||  |_)  |
    \   \    |   __|   /  /_\  \   |  |     |  |  |  ||   _  <
.----)   |   |  |____ /  _____  \  |  |____ |  '--'  ||  |_)  |
|_______/    |_______/__/     \__\ |_______||_______/ |______/
`}

func GetBanner() *string {
	rand.Seed(time.Now().UnixNano())
	no := rand.Intn(3)
	return &banners[no]
}
