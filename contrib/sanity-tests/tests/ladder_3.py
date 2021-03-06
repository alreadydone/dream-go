#!/usr/bin/env python3

from test_suite import *

with TestCase('Broken Ladder 3') as ladder_3:
    #
    #      a b c d e f g h j k l m n o p q r s t
    #    ╭───────────────────────────────────────╮
    # 19 │                                       │ 19
    # 18 │                       ○   ○ ●         │ 18
    # 17 │           ○   ●               ● ○ ●   │ 17
    # 16 │       ○           ●   ●   ○   ● ○ ●   │ 16
    # 15 │                             ● ● ● ○   │ 15
    # 14 │     ○             ○   ○   ○   ○ ○   ○ │ 14
    # 13 │                                   ○   │ 13
    # 12 │                       ●   ●   ● ● ○   │ 12
    # 11 │                             ● ○ ○ ●   │ 11
    # 10 │                                 ●     │ 10
    #  9 │                                       │ 9
    #  8 │                                       │ 8
    #  7 │   ●                                   │ 7
    #  6 │ ●   ●                                 │ 6
    #  5 │   ● ○ ○                               │ 5
    #  4 │   ○ ● ○                   ●   ●       │ 4
    #  3 │   ○ ● ○                               │ 3
    #  2 │                                       │ 2
    #  1 │                                       │ 1
    #    ╰───────────────────────────────────────╯
    #      a b c d e f g h j k l m n o p q r s t
    #     ● Black    ○ White
    #
    ladder_3.setup_from_sgf('examples/ladder_3.sgf', 54)
    ladder_3.genmove('W').expect_to(not_be_vertex('W', 'q10'))
