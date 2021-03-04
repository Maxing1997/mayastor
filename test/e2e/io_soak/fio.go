package io_soak

import (
	"e2e-basic/common"

	"fmt"
	"io/ioutil"
	"time"

	logf "sigs.k8s.io/controller-runtime/pkg/log"
)

// This table of duty cycles is guesstimates and bear no relation to real loads.
// TODO: make configurable
var FioDutyCycles = []struct {
	thinkTime       int
	thinkTimeBlocks int
}{
	{500000, 1000},  // 0.5 second, 1000 blocks
	{750000, 1000},  // 0.75 second, 1000 blocks
	{1000000, 2000}, // 1 second, 2000 blocks
	{1250000, 2000}, // 1.25 seconds, 2000 blocks
	{1500000, 3000}, // 1.5  seconds, 3000 blocks
	{1750000, 3000}, // 1.75  seconds, 3000 blocks
	{2000000, 4000}, // 2  seconds, 4000 blocks
}

const fixedDuration = 60

// see https://fio.readthedocs.io/en/latest/fio_doc.html#i-o-rate
// run fio in a loop of fixed duration to fulfill a larger duration,
// this to facilitate a relatively timely termination when an error
// occurs elsewhere.
// podName - name of the fio pod
// duration - time in seconds to run fio
// thinktime -  usecs, stall the job for the specified period of time after an I/O has completed before issuing the next
// thinktime_blocks - how many blocks to issue, before waiting thinktime usecs.
// rawBlock - false for filesystem volumes, true for raw block mounts.
func RunIoSoakFio(podName string, duration time.Duration, thinkTime int, thinkTimeBlocks int, rawBlock bool, doneC chan<- string, errC chan<- error) {
	secs := int(duration.Seconds())
	argThinkTime := fmt.Sprintf("--thinktime=%d", thinkTime)
	argThinkTimeBlocks := fmt.Sprintf("--thinktime_blocks=%d", thinkTimeBlocks)

	logf.Log.Info("Running fio",
		"pod", podName,
		"duration", duration,
		"thinktime", thinkTime,
		"thinktime_blocks", thinkTimeBlocks,
		"rawBlock", rawBlock,
	)

	fioFile := ""
	if rawBlock {
		fioFile = common.FioBlockFilename
	} else {
		fioFile = common.FioFsFilename
	}

	for ix := 1; secs > 0; ix++ {
		runtime := fixedDuration
		if runtime > secs {
			runtime = secs
		}
		secs -= runtime

		logf.Log.Info("run fio ",
			"iteration", ix,
			"pod", podName,
			"duration", runtime,
			"thinktime", thinkTime,
			"thinktime_blocks", thinkTimeBlocks,
			"rawBlock", rawBlock,
			"fioFile", fioFile,
		)
		output, err := common.RunFio(podName, runtime, fioFile, argThinkTime, argThinkTimeBlocks )

		//TODO: for now shove the output into /tmp
		_ = ioutil.WriteFile("/tmp/"+podName+".out", output, 0644)
		//logf.Log.Info(string(output))
		if err != nil {
			logf.Log.Info("Abort running fio", "pod", podName, "error", err)
			errC <- err
			return
		}
	}
	logf.Log.Info("Finished running fio", "pod", podName, "duration", duration)
	doneC <- podName
}