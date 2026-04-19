package main

import (
	"bytes"
	"encoding/binary"
	"flag"
	"fmt"
	"io"
	"os"

	silk "github.com/sjzar/go-silk"
)

const (
	defaultSampleRate = 24000
	channelCount      = 1
	bitsPerSample     = 16
)

func main() {
	sampleRate := flag.Int("sample-rate", defaultSampleRate, "output WAV sample rate")
	flag.Parse()

	input, err := io.ReadAll(os.Stdin)
	if err != nil {
		fatalf("read silk stdin: %v", err)
	}
	if len(input) == 0 {
		fatalf("silk input is empty")
	}

	var pcm bytes.Buffer
	writer := silk.NewWriter(&pcm)
	writer.Decoder.SetSampleRate(*sampleRate)

	if _, err := io.Copy(writer, bytes.NewReader(input)); err != nil {
		fatalf("decode silk stream: %v", err)
	}
	if err := writer.Close(); err != nil {
		fatalf("flush silk stream: %v", err)
	}
	if pcm.Len() == 0 {
		fatalf("decoded PCM is empty")
	}

	if err := writeWAV(os.Stdout, pcm.Bytes(), *sampleRate); err != nil {
		fatalf("write wav: %v", err)
	}
}

func writeWAV(out io.Writer, pcm []byte, sampleRate int) error {
	const (
		fmtChunkSize   = 16
		wavHeaderSize  = 44
		audioFormatPCM = 1
	)

	byteRate := sampleRate * channelCount * bitsPerSample / 8
	blockAlign := channelCount * bitsPerSample / 8
	dataSize := len(pcm)
	riffSize := wavHeaderSize - 8 + dataSize

	writeString := func(s string) error {
		_, err := io.WriteString(out, s)
		return err
	}
	writeU16 := func(v uint16) error {
		return binary.Write(out, binary.LittleEndian, v)
	}
	writeU32 := func(v uint32) error {
		return binary.Write(out, binary.LittleEndian, v)
	}

	if err := writeString("RIFF"); err != nil {
		return err
	}
	if err := writeU32(uint32(riffSize)); err != nil {
		return err
	}
	if err := writeString("WAVE"); err != nil {
		return err
	}
	if err := writeString("fmt "); err != nil {
		return err
	}
	if err := writeU32(fmtChunkSize); err != nil {
		return err
	}
	if err := writeU16(audioFormatPCM); err != nil {
		return err
	}
	if err := writeU16(channelCount); err != nil {
		return err
	}
	if err := writeU32(uint32(sampleRate)); err != nil {
		return err
	}
	if err := writeU32(uint32(byteRate)); err != nil {
		return err
	}
	if err := writeU16(uint16(blockAlign)); err != nil {
		return err
	}
	if err := writeU16(bitsPerSample); err != nil {
		return err
	}
	if err := writeString("data"); err != nil {
		return err
	}
	if err := writeU32(uint32(dataSize)); err != nil {
		return err
	}
	_, err := out.Write(pcm)
	return err
}

func fatalf(format string, args ...any) {
	fmt.Fprintf(os.Stderr, format+"\n", args...)
	os.Exit(1)
}
