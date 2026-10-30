"""Shared refusal for a native upgrade dialog; drivers never confirm installers."""


def update_prompt_visible(output):
    return b'Update available' in output and b'Update now' in output
