import time
import argparse
import sx126x

# --- LoRa Hat Setup ---
# IMPORTANT: Make sure you have enabled the serial port on your Raspberry Pi
# and that the jumpers M0 and M1 are removed from the HAT.
# You can use `sudo raspi-config` to enable the serial port.
node = sx126x.sx126x(
    serial_num="/dev/ttyS0",
    freq=868,
    addr=0,
    power=22,
    rssi=True,
    air_speed=2400,
    relay=False,
)


def send_message(message):
    """
    Sends a message.
    """
    # The Waveshare library expects a specific format for sending messages.
    # It needs the destination address, frequency, and the payload.
    # For simplicity, we will send to a fixed address (e.g., address 1)
    # and use the same frequency as the node.
    dst_addr = 1
    offset_frequence = node.offset_freq

    data = (
        bytes([dst_addr >> 8])
        + bytes([dst_addr & 0xFF])
        + bytes([offset_frequence])
        + bytes([node.addr >> 8])
        + bytes([node.addr & 0xFF])
        + bytes([node.offset_freq])
        + message.encode()
    )

    print(f"Sending LoRa message: {message}")
    node.send(data)
    print("Message sent.")


def receive_messages():
    """
    Receives messages continuously.
    """
    print("Starting LoRa receiver. Press Ctrl+C to exit.")
    while True:
        try:
            message = node.receive()
            if message:
                # The message from the waveshare library is a tuple
                # The actual message is the last element
                message_payload = message[-1]
                print(f"Received message: {message_payload}")
            time.sleep(0.1)
        except KeyboardInterrupt:
            print("\nExiting receiver mode.")
            break


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description="LoRa CLI application")
    parser.add_argument(
        "mode", choices=["send", "receive"], help="The mode to run the application in."
    )
    parser.add_argument(
        "message",
        nargs="?",
        default=None,
        help="The message to send (only in send mode).",
    )

    args = parser.parse_args()

    if args.mode == "send":
        if args.message:
            send_message(args.message)
        else:
            print("Error: Message argument is required for send mode.")
    elif args.mode == "receive":
        receive_messages()
