# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License in the LICENSE file at the
# root of this repository, or online at
#
#     http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.

MZFROM ubuntu-base

RUN apt-get update && apt-get install -qy maven

# Package twice to take advantage of Docker caching for local development.
# The first packaging only uses pom.xml as input.

COPY pom.xml /scratch/pom.xml
RUN cd /scratch && mvn package

COPY . /scratch
RUN cd /scratch && mvn -o package

MZFROM ubuntu-base

RUN apt-get update && apt-get install -qy openjdk-11-jre-headless

COPY --from=0 /scratch/target/tb-0.1-SNAPSHOT.jar /usr/local/share/java/tb.jar

ENTRYPOINT ["/usr/bin/java", "-jar", "/usr/local/share/java/tb.jar"]

CMD ["-h"]
