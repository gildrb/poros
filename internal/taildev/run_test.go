package taildev

import (
	"os/exec"
	"syscall"
	"testing"
	"time"
)

func TestStopRemainingProcessGroupEscalates(t *testing.T) {
	child := exec.Command("sh", "-c", "trap '' TERM; sleep 60 & exit 0")
	child.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	if err := child.Start(); err != nil {
		t.Fatal(err)
	}
	if err := child.Wait(); err != nil {
		t.Fatal(err)
	}

	stopRemainingProcessGroup(child, syscall.SIGTERM, time.Now().Add(100*time.Millisecond))
	if processGroupExists(child.Process.Pid) {
		t.Fatal("child process group still exists after cleanup")
	}
}
