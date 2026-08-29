import unittest

from textlib.lru import LRUCache


class TestLRUHidden(unittest.TestCase):
    def test_recency_chain_with_interleaved_gets(self):
        c = LRUCache(3)
        for k in ("a", "b", "c"):
            c.put(k, k.upper())
        self.assertEqual(c.get("a"), "A")  # order now b, c, a
        c.put("d", "D")  # evicts b
        self.assertNotIn("b", c)
        self.assertIn("a", c)
        self.assertIn("c", c)
        self.assertIn("d", c)
        self.assertEqual(len(c), 3)

    def test_repeated_gets_keep_entry_alive(self):
        c = LRUCache(2)
        c.put("keep", 1)
        c.put("old", 2)
        for _ in range(5):
            self.assertEqual(c.get("keep"), 1)
        c.put("new", 3)
        self.assertIn("keep", c)
        self.assertNotIn("old", c)


if __name__ == "__main__":
    unittest.main()
