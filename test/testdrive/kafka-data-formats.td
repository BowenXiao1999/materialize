# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

$ kafka-create-topic topic=input_csv
$ kafka-create-topic topic=input_csv_partitioned partitions=2
$ kafka-create-topic topic=input_proto

> CREATE MATERIALIZED SOURCE input_csv (first, second)
  FROM KAFKA BROKER '${testdrive.kafka-addr}' TOPIC 'testdrive-input_csv-${testdrive.seed}'
  FORMAT CSV WITH 2 COLUMNS;

$ kafka-ingest format=bytes topic=input_csv
1,2
2,3

> SELECT * from input_csv;
first second mz_offset
----------------------
1     2      1
2     3      2

$ kafka-ingest format=bytes topic=input_csv_partitioned partition=0
1,2

$ kafka-ingest format=bytes topic=input_csv_partitioned partition=1
2,3

> CREATE MATERIALIZED SOURCE input_csv_partitioned (first, second)
  FROM KAFKA BROKER '${testdrive.kafka-addr}' TOPIC 'testdrive-input_csv_partitioned-${testdrive.seed}'
  WITH (start_offset=[1,0])
  FORMAT CSV WITH 2 COLUMNS;

> SELECT * FROM input_csv_partitioned
first second mz_offset
----------------------
2     3      1
