# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

$ set names-schema={
    "type": "record",
    "name": "envelope",
    "fields": [
        {
            "name": "before",
            "type": [
                {
                    "name": "row",
                    "type": "record",
                    "fields": [
                        {"name": "num", "type": "long"},
                        {"name": "name", "type": "string"}
                    ]
                },
                "null"
            ]
        },
        { "name": "after", "type": ["row", "null"] }
    ]
  }

$ kafka-create-topic topic=names

$ kafka-ingest format=avro topic=names schema=${names-schema} timestamp=1
{"before": null, "after": {"row":{"num": 1, "name": "one"}}}
{"before": null, "after": {"row":{"num": 2, "name": "two"}}}
{"before": null, "after": {"row":{"num": 3, "name": "three"}}}

$ set mods-schema={
    "type": "record",
    "name": "envelope",
    "fields": [
        {
            "name": "before",
            "type": [
                {
                    "name": "row",
                    "type": "record",
                    "fields": [
                        {"name": "num", "type": "long"},
                        {"name": "mod", "type": "string"}
                    ]
                },
                "null"
            ]
        },
        { "name": "after", "type": ["row", "null"] }
    ]
  }

$ kafka-create-topic topic=mods

$ kafka-ingest format=avro topic=mods schema=${mods-schema} timestamp=1
{"before": null, "after": {"row":{"num": 0, "mod": "even"}}}
{"before": null, "after": {"row":{"num": 1, "mod": "odd"}}}
{"before": null, "after": {"row":{"num": 2, "mod": "even"}}}

$ set plurals-schema={
    "type": "record",
    "name": "envelope",
    "fields": [
        {
            "name": "before",
            "type": [
                {
                    "name": "row",
                    "type": "record",
                    "fields": [
                        {"name": "num", "type": "string"},
                        {"name": "noun", "type": "string"}
                    ]
                },
                "null"
            ]
        },
        { "name": "after", "type": ["row", "null"] }
    ]
  }

$ kafka-create-topic topic=plurals

$ kafka-ingest format=avro topic=plurals schema=${plurals-schema} timestamp=1
{"before": null, "after": {"row":{"num": "one", "noun": "sheep"}}}
{"before": null, "after": {"row":{"num": "two", "noun": "sheep"}}}
{"before": null, "after": {"row":{"num": "one", "noun": "mouse"}}}
{"before": null, "after": {"row":{"num": "two", "noun": "meeses"}}}

$ kafka-ingest format=avro topic=names schema=${names-schema} timestamp=2
{"before": null, "after": null}

$ kafka-ingest format=avro topic=mods schema=${mods-schema} timestamp=2
{"before": null, "after": null}

$ kafka-ingest format=avro topic=plurals schema=${plurals-schema} timestamp=2
{"before": null, "after": null}

> CREATE MATERIALIZED SOURCE names
  FROM KAFKA BROKER '${testdrive.kafka-addr}' TOPIC 'testdrive-names-${testdrive.seed}'
  FORMAT AVRO USING SCHEMA '${names-schema}'
  ENVELOPE DEBEZIUM

> CREATE MATERIALIZED SOURCE mods
  FROM KAFKA BROKER '${testdrive.kafka-addr}' TOPIC 'testdrive-mods-${testdrive.seed}'
  FORMAT AVRO USING SCHEMA '${mods-schema}'
  ENVELOPE DEBEZIUM

> CREATE MATERIALIZED SOURCE plurals
  FROM KAFKA BROKER '${testdrive.kafka-addr}' TOPIC 'testdrive-plurals-${testdrive.seed}'
  FORMAT AVRO USING SCHEMA '${plurals-schema}'
  ENVELOPE DEBEZIUM

> CREATE MATERIALIZED VIEW test1 (names_num, names_name, mods_num, mods_mod, nouns_num, nouns_noun) AS
  SELECT * FROM names, mods, plurals WHERE names.num = mods.num AND names.name = plurals.num;

> SELECT * FROM test1;
names_num names_name mods_num mods_mod nouns_num nouns_noun
-----------------------------------------------------------
1 one 1 odd one sheep
1 one 1 odd one mouse
2 two 2 even two sheep
2 two 2 even two meeses

> CREATE MATERIALIZED VIEW test2 (names_num, names_name, mods_num, mods_mod, plurals_num, plurals_noun) AS
  SELECT * FROM names, mods, plurals
  WHERE names.num = mods.num AND names.name = plurals.num AND plurals.num = 'one';

> SELECT * FROM test2;
names_num names_name mods_num mods_mod plurals_num plurals_noun
----------------------------------------------------------------
1 one 1 odd one sheep
1 one 1 odd one mouse

> CREATE MATERIALIZED VIEW test3 (names_num, names_name, mods_num, mods_mod, plurals_num, plurals_noun) AS
  SELECT * FROM names
  RIGHT JOIN mods ON names.num = mods.num
  LEFT JOIN plurals ON names.name = plurals.num;

> SELECT * FROM test3;
names_num names_name mods_num mods_mod plurals_num plurals_noun
---------------------------------------------------------------
<null> <null> 0 even <null> <null>
1 one 1 odd one sheep
1 one 1 odd one mouse
2 two 2 even two sheep
2 two 2 even two meeses

> CREATE MATERIALIZED VIEW test4 (names_num, names_name, mods_num, mods_mod, plurals_num, plurals_noun) AS
  SELECT * FROM names, mods, plurals as foo
  WHERE names.num = mods.num AND names.name = foo.num AND foo.num = 'one';

> SELECT * FROM test4;
names_num names_name mods_num mods_mod plurals_num plurals_noun
---------------------------------------------------------------
1 one 1 odd one sheep
1 one 1 odd one mouse
