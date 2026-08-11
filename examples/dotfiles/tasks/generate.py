from wombat import output, params

from helpers.message import render


destination = output / "wombat-generated" / "task.txt"
destination.parent.mkdir(parents=True)
destination.write_text(render(params["message"]))
