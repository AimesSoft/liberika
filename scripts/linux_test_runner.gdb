set confirm off
set pagination off
set disable-randomization off
set print thread-events off
run
if $_thread != 0
  thread apply all bt 32
  info sharedlibrary
  quit 139
end
