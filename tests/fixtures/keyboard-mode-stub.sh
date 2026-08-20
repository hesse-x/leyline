#!/bin/sh

# Mimic a TUI which enables Kitty keyboard handling and is then suspended by Ctrl+Z.
printf '\033[=3u'
sleep 0.2
kill -TSTP "$$"
