from src.config import load_settings


def get_timeout():
    return load_settings()["old_timeout"]
