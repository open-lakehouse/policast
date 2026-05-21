package com.policast.spark

import org.apache.spark.api.plugin.{DriverPlugin, ExecutorPlugin, SparkPlugin}

/**
 * SparkPlugin entry point for policast-cel governance.
 *
 * Register via:
 *   spark.plugins = com.policast.spark.PolicastPlugin
 *
 * Also register the Catalyst extensions:
 *   spark.sql.extensions = com.policast.spark.PolicastExtensions
 */
class PolicastPlugin extends SparkPlugin {
  override def driverPlugin(): DriverPlugin = new PolicastDriverPlugin()
  override def executorPlugin(): ExecutorPlugin = null
}
