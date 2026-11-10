import secrets
import sys
import time

first = secrets.token_hex(8)
print("FIRST>", first, flush=True)
if input().strip() != first:
    sys.exit("first answer was incorrect")

time.sleep(2)
second = secrets.token_hex(8)
print("SECOND>", second, flush=True)
if input().strip() != second:
    sys.exit("second answer was incorrect")

print("INTERACTIVE_OK", flush=True)
