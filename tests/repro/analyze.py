# -*- coding: utf-8 -*-
from __future__ import print_function
import re
import sys

data = open(sys.argv[1], "rb").read().decode("utf-8", "replace")
frames = [(m.start(), m.group(1)) for m in re.finditer(u"\u00b7 (\d+)s", data)]
notices = [(m.start(), m.group(1)) for m in re.finditer(u"attempt (\d+)", data)]

print("footer_frames=%d" % len(frames))
print("elapsed_seq=" + ",".join(v for _, v in frames))
if frames:
    nums = [int(v) for _, v in frames]
    print("max_elapsed=%ds" % max(nums))
    print("distinct=" + ",".join(str(d) for d in sorted(set(nums))))
if notices:
    print("retry_attempts=" + ",".join(v for _, v in notices))

if not frames:
    print("verdict=NO-FOOTER (no spinner frames captured)")
elif not notices:
    print("verdict=NO-RETRY (no retry notices; spinner ran but nothing to update)")
else:
    last_frame_pos = frames[-1][0]
    later = [v for p, v in notices if p > last_frame_pos]
    max_e = int(frames[-1][1])
    if later and max_e <= 2:
        print("verdict=FROZEN (footer stuck at %ss; retries %s continued with no repaint)"
              % (frames[-1][1], ",".join(later)))
    else:
        print("verdict=TICKING (footer advanced to %ss alongside retries)" % max_e)
