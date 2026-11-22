#!/usr/bin/env python3
"""Catalog invalidity must stop reporting even under optimized Python."""
import copy
import importlib.util
import json
from pathlib import Path
import unittest

SCRIPT = Path(__file__).with_name("terminal-family-product-catalog.py")
SPEC = importlib.util.spec_from_file_location("product_catalog", SCRIPT)
catalog = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(catalog)


class ProductCatalogTests(unittest.TestCase):
    def setUp(self):
        self.data = json.loads(catalog.CATALOG.read_text())

    def test_valid_catalog(self):
        catalog.validate(self.data)

    def test_runtime_promotion_is_rejected(self):
        row = self.data["journeys"][0]
        row["adapters"]["aether"]["runtime"] = "pass"
        with self.assertRaisesRegex(ValueError, "invalid runtime scope"):
            catalog.validate(self.data)
        row["adapters"]["aether"]["runtime"] = "not-run"
        row["test"]["status"] = "pass"
        with self.assertRaisesRegex(ValueError, "cannot award native pass"):
            catalog.validate(self.data)

    def test_duplicate_journey_is_rejected(self):
        self.data["journeys"].append(copy.deepcopy(self.data["journeys"][0]))
        with self.assertRaisesRegex(ValueError, "duplicate journey IDs"):
            catalog.validate(self.data)

    def test_unproven_target_must_have_work_owner(self):
        row = next(row for row in self.data["journeys"] if row["id"] == "PRODUCT.NOTEBOOK.001")
        row["target_adapters"].remove("frost")
        with self.assertRaisesRegex(ValueError, "unproven lane has no work target"):
            catalog.validate(self.data)

    def test_paths_cannot_fetch_outside_declared_objects(self):
        self.data["journeys"][0]["service"]["path"] = "../anything"
        with self.assertRaisesRegex(ValueError, "invalid repository-relative"):
            catalog.validate(self.data)


if __name__ == "__main__":
    unittest.main()
