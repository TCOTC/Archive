package main

import (
	"os"

	"github.com/MerlinKodo/clash-rev/component/cli"
)

func main() {
	app := cli.NewApp()
	if err := app.Run(); err != nil {
		os.Exit(1)
	}
}
