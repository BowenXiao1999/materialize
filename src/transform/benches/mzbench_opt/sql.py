# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

import logging
import re
from pathlib import Path
from typing import Any, Dict, List, Optional

import psycopg2
import psycopg2.extensions
import sqlparse
from psycopg2.extensions import ISOLATION_LEVEL_AUTOCOMMIT

from . import Scenario, util


class Query:
    """An API for manipulating workload queries."""

    def __init__(self, query) -> None:
        self.query = query

    def __str__(self) -> str:
        return self.query

    def name(self) -> str:
        """Extracts and returns the name of this query from a '-- name: {name}' comment.
        Returns 'anonymous' if the name is not set."""
        p = r"-- name\: (?P<name>.+)"
        m = re.search(p, self.query, re.MULTILINE)
        return m.group("name") if m else "anonoymous"

    def explain(self, timing: bool) -> str:
        """Prepends 'EXPLAIN (TIMING {timing}) PLAN FOR' to the query."""
        return "\n".join([f"EXPLAIN (TIMING {bool(timing)}) PLAN FOR", self.query])


class ExplainOutput:
    """An API for manipulating 'EXPLAIN ... PLAN FOR' results."""

    def __init__(self, output) -> None:
        self.output = output

    def __str__(self) -> str:
        return self.output

    def decorrelation_time(self) -> Optional[int]:
        """Optionally, returns the decorrelation_time for an 'EXPLAIN (TIMING true)' output."""
        p = r"Decorrelation time\: (?P<time>[0-9]{2}\:[0-9]{2}\:[0-9]{2}\.[0-9]+)"
        m = re.search(p, self.output, re.MULTILINE)
        return util.str_to_ns(m.group("time")) if m else None

    def optimization_time(self) -> Optional[int]:
        """Optionally, returns the optimization_time time for an 'EXPLAIN (TIMING true)' output."""
        p = r"Optimization time\: (?P<time>[0-9]{2}\:[0-9]{2}\:[0-9]{2}\.[0-9]+)"
        m = re.search(p, self.output, re.MULTILINE)
        return util.str_to_ns(m.group("time")) if m else None


class Database:
    """An API to the database under test."""

    def __init__(
        self,
        port: int,
        host: str,
        user: str,
    ) -> None:
        kwargs = dict(
            host=host,
            port=port,
            user=user,
        )
        logging.debug(f"Initialize Database with connection={kwargs}")
        self.conn = psycopg2.connect(**kwargs)
        self.conn.set_isolation_level(ISOLATION_LEVEL_AUTOCOMMIT)

    def mz_version(self) -> str:
        result = self.query_one("SELECT mz_version()")
        return result[0]

    def drop_database(self, scenario: Scenario) -> None:
        logging.debug(f'Drop database "{scenario}"')
        self.execute(f"DROP DATABASE IF EXISTS {scenario}")

    def create_database(self, scenario: Scenario) -> None:
        logging.debug(f'Create database "{scenario}"')
        self.execute(f"CREATE DATABASE {scenario}")

    def set_database(self, scenario: Scenario) -> None:
        logging.debug(f'Set default database to "{scenario}"')
        self.execute(f"SET DATABASE = {scenario}")

    def explain(self, query: Query, timing: bool) -> "ExplainOutput":
        result = self.query_one(query.explain(timing))
        return ExplainOutput(result[0])

    def execute(self, statement: str) -> None:
        cursor = self.conn.cursor()
        cursor.execute(statement)

    def execute_all(self, statements: List[str]) -> None:
        for statement in statements:
            self.execute(statement)

    def query_one(self, query: str) -> Dict[Any, Any]:
        cursor = self.conn.cursor()
        cursor.execute(query)
        return cursor.fetchone()

    def query_all(self, query: str) -> Dict[Any, Any]:
        cursor = self.conn.cursor()
        cursor.execute(query)
        return cursor.fetchall()


# Utility functions
# -----------------


def parse_from_file(path: Path) -> List[Query]:
    """Parses a *.sql file to a list of queries."""
    return sqlparse.split(path.read_text())
