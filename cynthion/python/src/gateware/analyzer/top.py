#!/usr/bin/env python3
# pylint: disable=maybe-no-member
#
# This file is part of Cynthion.
#
# Copyright (c) 2020-2023 Great Scott Gadgets <info@greatscottgadgets.com>
# SPDX-License-Identifier: BSD-3-Clause

from luna.gateware.applets.analyzer import USBAnalyzerApplet
from luna import top_level_cli

if __name__ == "__main__":
    top_level_cli(USBAnalyzerApplet)
