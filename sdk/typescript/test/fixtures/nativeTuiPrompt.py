"""Shared refusal for a native upgrade dialog; drivers never confirm installers."""


def update_prompt_visible(output):
    # Terminal writes can split the selected option. Refuse on the title before
    # typing any close keys, rather than waiting for a complete dialog frame.
    return b'Update available' in output
