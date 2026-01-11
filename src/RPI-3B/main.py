#!/bin/python3

import serial
import time

# -------------------------------------------------
# Open a serial port
ser = serial.Serial(
    port="/dev/ttyS0",  # Windows example; use '/dev/ttyUSB0' on Linux/macOS
    baudrate=9600,
    bytesize=serial.EIGHTBITS,
    parity=serial.PARITY_NONE,
    stopbits=serial.STOPBITS_ONE,
    timeout=1,  # seconds; None = block forever
)

# -------------------------------------------------
# Write data
msg = b"Hello, device!\n"
ser.write(b"0xC1 05 01")  # send bytes
print(f"Sent: {msg}")

# -------------------------------------------------
# Read data (blocking until timeout)
response = ser.readline()  # reads up to newline or timeout
print(f"Received: {response}")

while True:
    continue


# -------------------------------------------------
# Non‑blocking read loop
while True:
    if ser.in_waiting:  # bytes waiting in the input buffer
        data = ser.read(ser.in_waiting)
        print("Got:", data)
    time.sleep(0.1)  # avoid busy‑wait

# -------------------------------------------------
# Clean up
ser.close()
