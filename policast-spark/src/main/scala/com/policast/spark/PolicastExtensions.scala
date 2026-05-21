package com.policast.spark

import org.apache.spark.sql.SparkSessionExtensions

/**
 * Register policast governance rules as Catalyst optimizer extensions.
 *
 * Usage:
 *   spark.sql.extensions = com.policast.spark.PolicastExtensions
 */
class PolicastExtensions extends (SparkSessionExtensions => Unit) {
  override def apply(extensions: SparkSessionExtensions): Unit = {
    extensions.injectOptimizerRule(session => new PolicastRowFilterRule(session))
    extensions.injectOptimizerRule(session => new PolicastColumnMaskRule(session))
  }
}
