from wombat import cache, params

from helpers.message import message


(cache / "prepared.txt").write_text(message(params["mode"]))
print(message(params["mode"]), end="")
