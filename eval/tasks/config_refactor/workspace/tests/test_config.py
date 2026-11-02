import unittest

from src.app import get_timeout
from src.config import load_settings


class ConfigTest(unittest.TestCase):
    def test_timeout_key_is_canonical(self):
        settings = load_settings()
        self.assertEqual(settings["timeout"], 30)
        self.assertNotIn("old_timeout", settings)

    def test_app_uses_canonical_key(self):
        self.assertEqual(get_timeout(), 30)


if __name__ == "__main__":
    unittest.main()
