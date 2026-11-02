import unittest

from src.cache import Cache


class CacheTest(unittest.TestCase):
    def test_update_existing_key_does_not_evict_another(self):
        cache = Cache(2)
        cache.put("a", 1)
        cache.put("b", 2)
        cache.put("b", 20)
        self.assertEqual(cache.get("a"), 1)
        self.assertEqual(cache.get("b"), 20)

    def test_evicts_oldest_insertion(self):
        cache = Cache(2)
        cache.put("a", 1)
        cache.put("b", 2)
        cache.put("c", 3)
        self.assertIsNone(cache.get("a"))
        self.assertEqual(cache.get("b"), 2)
        self.assertEqual(cache.get("c"), 3)


if __name__ == "__main__":
    unittest.main()
