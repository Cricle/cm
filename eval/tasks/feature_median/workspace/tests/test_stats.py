import unittest

from src.stats import mean, median


class StatsTest(unittest.TestCase):
    def test_mean_still_works(self):
        self.assertEqual(mean([1, 2, 3]), 2)

    def test_median_odd(self):
        self.assertEqual(median([3, 1, 4]), 3)

    def test_median_even(self):
        self.assertEqual(median([4, 1, 3, 2]), 2.5)


if __name__ == "__main__":
    unittest.main()
