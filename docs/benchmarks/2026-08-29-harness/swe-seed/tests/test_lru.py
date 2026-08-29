import unittest

from textlib.lru import LRUCache


class TestLRUCache(unittest.TestCase):
    def test_put_get_roundtrip(self):
        c = LRUCache(2)
        c.put("a", 1)
        c.put("b", 2)
        self.assertEqual(c.get("a"), 1)
        self.assertEqual(c.get("b"), 2)

    def test_capacity_evicts_oldest_insert(self):
        c = LRUCache(2)
        c.put("a", 1)
        c.put("b", 2)
        c.put("c", 3)
        self.assertNotIn("a", c)
        self.assertIn("b", c)
        self.assertIn("c", c)
        self.assertEqual(len(c), 2)

    def test_get_refreshes_recency(self):
        c = LRUCache(2)
        c.put("a", 1)
        c.put("b", 2)
        self.assertEqual(c.get("a"), 1)  # a is now most recent
        c.put("c", 3)  # evicts b, not a
        self.assertIn("a", c)
        self.assertNotIn("b", c)
        self.assertIn("c", c)

    def test_overwrite_refreshes_recency(self):
        c = LRUCache(2)
        c.put("a", 1)
        c.put("b", 2)
        c.put("a", 10)
        c.put("c", 3)
        self.assertIn("a", c)
        self.assertEqual(c.get("a"), 10)
        self.assertNotIn("b", c)

    def test_get_missing_returns_default(self):
        c = LRUCache(1)
        self.assertIsNone(c.get("nope"))
        self.assertEqual(c.get("nope", "dflt"), "dflt")

    def test_zero_capacity_raises(self):
        with self.assertRaises(ValueError):
            LRUCache(0)


if __name__ == "__main__":
    unittest.main()
