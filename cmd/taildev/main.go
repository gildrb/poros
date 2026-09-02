package main

import (
	"os"

	"github.com/gildrb/taildev/internal/taildev"
)

var version = "dev"

func main() {
	os.Exit(taildev.Run(os.Args[1:], os.Stdout, os.Stderr, version))
}
